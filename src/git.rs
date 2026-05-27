//! Centralized, instrumented invocation of the system `git` binary.
//!
//! Every subprocess call to `git` routes through [`run`] (or a wrapper built on
//! it) so each invocation shows up as a `git` span in the per-run trace —
//! carrying the subcommand, working directory, exit code and wall time — and
//! fails with a consistent error that quotes git's own stderr rather than a
//! bare exit status. gitaur already depends on the `git` binary at runtime (it
//! drives worktree materialization and PKGBUILD diffs); keeping the spawns in
//! one place is what makes them uniformly observable.

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use tracing::{debug, field, info_span};

use crate::error::{Error, Result};

/// Run `git <args>` in `cwd` (or the current directory when `None`), capturing
/// its output.
///
/// On success returns captured stdout. On a non-zero exit returns
/// [`Error::Gix`] carrying the trimmed stderr, so the message points at git's
/// actual complaint. The spawn itself failing (e.g. `git` not on `PATH`)
/// surfaces as [`Error::other`].
///
/// The whole call is wrapped in an `info`-level `git` span whose duration marks
/// the subprocess wall time in the trace; the exit code is recorded on the span
/// once known.
pub fn run<I, S>(args: I, cwd: Option<&Path>) -> Result<Vec<u8>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args.into_iter().map(|s| s.as_ref().to_owned()).collect();
    let subcommand = args
        .first()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cwd_display = cwd.map(|p| p.display().to_string()).unwrap_or_default();

    let span =
        info_span!("git", subcommand = %subcommand, cwd = %cwd_display, exit_code = field::Empty);
    let _guard = span.enter();
    let printable = args
        .iter()
        .map(|s| s.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    debug!(args = %printable, "running git");
    let started = Instant::now();

    let mut cmd = Command::new("git");
    cmd.args(&args);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    let out = cmd
        .output()
        .map_err(|e| Error::other(format!("spawn git: {e}")))?;

    span.record("exit_code", out.status.code().unwrap_or(-1));
    debug!(
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        ok = out.status.success(),
        "git finished"
    );

    if !out.status.success() {
        return Err(Error::Gix(format!(
            "git {} failed: {}",
            printable,
            String::from_utf8_lossy(&out.stderr).trim(),
        )));
    }
    Ok(out.stdout)
}

/// Build/refresh the commit-graph for the bare repository at `repo`.
///
/// The commit-graph lets the next fetch's negotiation read commit times via an
/// mmap'd binary search instead of inflating every commit object from the pack
/// — the dominant remaining cost of building the have-set on the AUR mirror.
/// `--split` keeps each call incremental (only new commits are appended, with
/// periodic auto-merge), so repeated `-Sy` runs stay cheap.
///
/// Best-effort: the graph is a read accelerator (gix treats its absence as
/// "no graph" and falls back to the ODB), so a failure here — an old `git`
/// without `commit-graph`, a read-only store, etc. — is logged and swallowed
/// rather than aborting the refresh.
pub fn write_commit_graph(repo: &Path) {
    let _span = info_span!("write_commit_graph").entered();
    if let Err(e) = run(
        [
            "commit-graph",
            "write",
            "--reachable",
            "--split",
            "--no-progress",
        ],
        Some(repo),
    ) {
        debug!(error = %e, "commit-graph write failed; continuing without it");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A bare git repo with a couple of commits, for commit-graph tests.
    ///
    /// Hermetic: per-repo `user.*` identity and `commit.gpgsign=false` so the
    /// commits don't depend on (or trigger) the host's global git config —
    /// notably GPG signing, which would otherwise prompt a real signer.
    fn repo_with_commits() -> TempDir {
        let dir = TempDir::new().unwrap();
        let p = dir.path();
        run(["init", "--bare", p.to_str().unwrap()], None).unwrap();
        // Drive commits through a throwaway worktree so the bare repo gains refs.
        let wt = TempDir::new().unwrap();
        run(
            ["clone", p.to_str().unwrap(), wt.path().to_str().unwrap()],
            None,
        )
        .unwrap();
        let w = Some(wt.path());
        run(["config", "user.email", "t@t"], w).unwrap();
        run(["config", "user.name", "t"], w).unwrap();
        run(["config", "commit.gpgsign", "false"], w).unwrap();
        run(["config", "tag.gpgsign", "false"], w).unwrap();
        run(["commit", "--allow-empty", "-m", "c1"], w).unwrap();
        run(["commit", "--allow-empty", "-m", "c2"], w).unwrap();
        run(["push", "origin", "HEAD:refs/heads/main"], w).unwrap();
        dir
    }

    #[test]
    fn run_captures_stdout() {
        let out = run(["--version"], None).unwrap();
        assert!(String::from_utf8_lossy(&out).starts_with("git version"));
    }

    #[test]
    fn run_surfaces_stderr_on_failure() {
        // `git` with a bogus subcommand exits non-zero and writes to stderr.
        let err = run(["definitely-not-a-subcommand"], None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed"), "got: {msg}");
    }

    #[test]
    fn write_commit_graph_produces_a_graph() {
        let dir = repo_with_commits();
        let info = dir.path().join("objects").join("info");
        // Best-effort writer: on success it leaves either a single-file
        // `commit-graph` or a split `commit-graphs/` chain.
        write_commit_graph(dir.path());
        let single = info.join("commit-graph").exists();
        let split = info.join("commit-graphs").exists();
        assert!(
            single || split,
            "expected a commit-graph under {}",
            info.display()
        );
        // And whatever was written must verify clean.
        run(["commit-graph", "verify"], Some(dir.path())).unwrap();
    }
}
