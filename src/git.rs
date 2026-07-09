//! Centralized, instrumented invocation of the system `git` binary.
//!
//! Every subprocess call to `git` routes through [`run`] (or a wrapper built on
//! it) so each invocation shows up as a `git` span in the per-run trace —
//! carrying the subcommand, working directory, exit code and wall time — and
//! fails with a consistent error that quotes git's own stderr rather than a
//! bare exit status. aurox already depends on the `git` binary at runtime (it
//! drives worktree materialization and PKGBUILD diffs); keeping the spawns in
//! one place is what makes them uniformly observable.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use gix::ObjectId;
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
    run_with_stdin(args, cwd, None)
}

/// As [`run`], but feed `stdin` to the subprocess (and close the pipe so git
/// sees EOF). For commands like `commit-graph write --stdin-commits` that read
/// a small payload from stdin and emit little on stdout — large stdin *and*
/// large stdout together could deadlock this write-then-read sequence, but no
/// such caller exists.
fn run_with_stdin<I, S>(args: I, cwd: Option<&Path>, stdin: Option<&[u8]>) -> Result<Vec<u8>>
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
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| Error::other(format!("spawn git: {e}")))?;
    if let Some(bytes) = stdin {
        // `take()` drops the handle after the write, closing the pipe so git
        // reaches EOF and stops reading.
        child
            .stdin
            .take()
            .expect("stdin piped above")
            .write_all(bytes)
            .map_err(|e| Error::other(format!("write git stdin: {e}")))?;
    }
    let out = child
        .wait_with_output()
        .map_err(|e| Error::other(format!("wait git: {e}")))?;

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
/// `new_commits` selects how git discovers what to add:
/// - `Some(oids)` → `--stdin-commits`: git ingests exactly those tips plus any
///   ancestors not already graphed. On an incremental `-Sy` that touched a
///   handful of refs this is the few-commit closure rather than a walk of all
///   ~155k refs. Pass the fetched ref tips here.
/// - `None` → `--reachable`: walk every ref. The right choice on a fresh clone
///   or full rebuild, where there is no delta and the whole history is new.
///
/// Best-effort: the graph is a read accelerator (gix treats its absence as
/// "no graph" and falls back to the ODB), so a failure here — an old `git`
/// without `commit-graph`, a read-only store, etc. — is logged and swallowed
/// rather than aborting the refresh.
pub fn write_commit_graph(repo: &Path, new_commits: Option<&[ObjectId]>) {
    let _span = info_span!("write_commit_graph").entered();
    let mut args = vec!["commit-graph", "write", "--split", "--no-progress"];
    let stdin = if let Some(oids) = new_commits {
        args.push("--stdin-commits");
        let mut buf = Vec::new();
        for oid in oids {
            buf.extend_from_slice(oid.to_hex().to_string().as_bytes());
            buf.push(b'\n');
        }
        Some(buf)
    } else {
        args.push("--reachable");
        None
    };
    if let Err(e) = run_with_stdin(args, Some(repo), stdin.as_deref()) {
        debug!(error = %e, "commit-graph write failed; continuing without it");
    }
}

/// Fold all loose refs into `packed-refs` for the bare repository at `repo`.
///
/// Each fetch writes its changed refs as loose files, which accumulate forever
/// on a mirror aurox only ever fetches. Loose refs miss gix's borrowed
/// packed-refs fast path (#4/#7/#9): each one costs an `open()` syscall in the
/// negotiate/update find loops and inflates the `loose_names` set every ref is
/// hashed against. Packing returns the whole store to the all-packed state
/// those fast paths assume.
///
/// Rewrites the entire `packed-refs` file (~1 s on the ~155k-ref AUR mirror),
/// so callers should gate this on a loose-ref threshold rather than running it
/// every fetch. Best-effort, same rationale as [`write_commit_graph`].
pub fn pack_refs(repo: &Path) {
    let _span = info_span!("pack_refs").entered();
    if let Err(e) = run(["pack-refs", "--all"], Some(repo)) {
        debug!(error = %e, "pack-refs failed; continuing unpacked");
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
        write_commit_graph(dir.path(), None);
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

    #[test]
    fn write_commit_graph_from_stdin_commits() {
        let dir = repo_with_commits();
        // Mirror the incremental `-Sy` path: hand the fetched tip oid to git via
        // `--stdin-commits` instead of letting it walk every ref.
        let head = run(["rev-parse", "refs/heads/main"], Some(dir.path())).unwrap();
        let oid = ObjectId::from_hex(String::from_utf8(head).unwrap().trim().as_bytes()).unwrap();
        write_commit_graph(dir.path(), Some(&[oid]));
        let info = dir.path().join("objects").join("info");
        assert!(
            info.join("commit-graph").exists() || info.join("commit-graphs").exists(),
            "expected a commit-graph under {}",
            info.display()
        );
        run(["commit-graph", "verify"], Some(dir.path())).unwrap();
    }
}
