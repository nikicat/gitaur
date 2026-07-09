//! Spawn `makepkg` under a pty so its `[[ -t 2 ]]` colour check passes.
//!
//! Under plain pipes makepkg goes monochrome — same gate as `ls`, `grep`, etc.
//! With a pty the child sees a terminal on stdout/stderr and emits the usual
//! coloured "==>" headers and curl progress bars. The bytes from the pty
//! master are tee'd live to the user's stdout and to `<worktree>/build.log`
//! for post-mortem debugging.
//!
//! Under a pty stdout and stderr merge into one stream — the same way the
//! user would see them running makepkg by hand. tracing output continues to
//! go to the parent's real stderr.
//!
//! stdin is not forwarded: the default args include `-d --noconfirm --needed`
//! and sudo for `pacman -U` is consolidated outside this function (see
//! `feedback_defer_consolidate_sudo`), so makepkg shouldn't read stdin during
//! a build. If something ever does (e.g. a PGP key prompt), it will hang
//! visibly rather than silently — surface for a future fix.

use crate::config::Config;
use crate::context;
use crate::error::{Error, Result};
use console::Term;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use portable_pty::{CommandBuilder, ExitStatus, PtySize, native_pty_system};
use signal_hook::consts::SIGINT;
use signal_hook::iterator::Signals;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::{debug, info, instrument, warn};

/// Run `makepkg` in `worktree` with the configured args + env, plus any
/// `extra_args` (e.g. `--nobuild` / `--noextract` for the VCS two-phase build).
///
/// makepkg has no flag to limit which pkgnames of a split PKGBUILD get
/// packaged — `package_*()` is run for every member, all-or-nothing. So we
/// always build the whole pkgbase; the caller-side install filter
/// (`build::filter_by_selection`) decides which of the resulting
/// `.pkg.tar.zst` files end up in the `pacman -U` transaction.
///
/// `fresh_log` truncates `build.log` (the first makepkg pass of a run);
/// passing `false` appends, so a multi-pass sequence (VCS `--nobuild` then
/// `--noextract`) lands in a single contiguous log.
///
/// Returns the path to the captured `build.log` on success; on failure the
/// same path is embedded in the [`Error::Build`] message.
#[instrument(skip(cfg))]
pub fn run(cfg: &Config, worktree: &Path, extra_args: &[&str], fresh_log: bool) -> Result<PathBuf> {
    let log_path = worktree.join("build.log");
    let log_file = if fresh_log {
        File::create(&log_path)?
    } else {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?
    };

    let pty = native_pty_system()
        .openpty(pty_size())
        .map_err(|e| Error::Build(format!("openpty: {e}")))?;

    let mut cmd = CommandBuilder::new(&cfg.makepkg_path);
    cmd.cwd(worktree);
    cmd.env("PKGDEST", worktree);
    cmd.env("SRCDEST", worktree.join("src-cache"));
    cmd.env("BUILDDIR", worktree);
    for arg in &cfg.makepkg_args {
        cmd.arg(arg);
    }
    for arg in extra_args {
        cmd.arg(arg);
    }
    debug!(args = ?cfg.makepkg_args, extra = ?extra_args, cwd = %worktree.display(), log = %log_path.display(), "spawning makepkg under pty");

    let mut child = pty
        .slave
        .spawn_command(cmd)
        .map_err(|e| Error::Build(format!("spawn makepkg: {e}")))?;
    // Drop our slave handle so master reads see EOF the instant the child
    // (the only other holder of the slave fds) exits.
    drop(pty.slave);
    let pid = child.process_id();

    let reader = pty
        .master
        .try_clone_reader()
        .map_err(|e| Error::Build(format!("pty reader: {e}")))?;

    // Catch SIGINT for the duration of this build. `Signals` installs an
    // async-signal-safe handler that suppresses the default action (which would
    // kill aurox) and feeds a blocking iterator instead — so a Ctrl+C mid-build
    // unwinds as `Error::Interrupted` and the no-arg loop bails back to the
    // table rather than the whole program dying. Dropping `Signals` at the end
    // of the function restores the previous disposition (RAII), so Ctrl+C on the
    // picker/table still exits.
    let mut signals = Signals::new([SIGINT])?;
    let handle = signals.handle();
    let interrupted = AtomicBool::new(false);

    let log = Mutex::new(log_file);
    let status = context::scope(|s| -> Result<ExitStatus> {
        s.spawn(|| tee(reader, std::io::stdout(), &log));
        // Watcher: blocks on the signal pipe (no polling). On Ctrl+C it notes
        // the interrupt and forwards SIGINT to makepkg's process group, then
        // loops back to block again; `handle.close()` below ends it once
        // makepkg has exited.
        s.spawn(|| {
            for _ in &mut signals {
                interrupted.store(true, Ordering::SeqCst);
                forward_sigint(pid);
            }
        });
        let status = child
            .wait()
            .map_err(|e| Error::Build(format!("wait makepkg: {e}")))?;
        handle.close();
        Ok(status)
    });
    let status = status?;

    if interrupted.load(Ordering::SeqCst) {
        warn!(log = %log_path.display(), "makepkg interrupted by SIGINT");
        return Err(Error::Interrupted);
    }
    if !status.success() {
        let code = status.exit_code();
        return Err(Error::Build(format!(
            "makepkg exited with status {code} (log: {})",
            log_path.display(),
        )));
    }
    info!(log = %log_path.display(), "makepkg succeeded");
    Ok(log_path)
}

/// Resolve the exact package filenames `makepkg` will produce in `worktree`,
/// *before* building, via `makepkg --packagelist`.
///
/// `--packagelist` sources the PKGBUILD and evaluates `pkgver()`, so a VCS
/// pkgbase (`-git`/`-svn`/…) reports its *dynamic* version here — the same
/// string the build stamps into the artifact filename. Freezing on this list
/// (instead of the static `.SRCINFO` pkgver, which `pkgver()` overrides and
/// which therefore never matches the built file) is what lets `build::run_build`
/// collect a VCS package's artifact instead of failing with "produced no
/// packages". The env mirrors [`run`], so sources land in the same `SRCDEST`
/// the subsequent build reuses — resolving the version here costs no extra
/// download beyond what the build would do anyway.
#[instrument(skip(cfg))]
pub fn package_list(cfg: &Config, worktree: &Path) -> Result<Vec<PathBuf>> {
    let out = Command::new(&cfg.makepkg_path)
        .arg("--packagelist")
        .current_dir(worktree)
        .env("PKGDEST", worktree)
        .env("SRCDEST", worktree.join("src-cache"))
        .env("BUILDDIR", worktree)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| Error::Build(format!("spawn makepkg --packagelist: {e}")))?;
    if !out.status.success() {
        return Err(Error::Build(format!(
            "makepkg --packagelist failed: {}",
            String::from_utf8_lossy(&out.stderr).trim(),
        )));
    }
    let list = parse_package_list(&String::from_utf8_lossy(&out.stdout));
    debug!(count = list.len(), "froze package list before build");
    Ok(list)
}

/// Parse `makepkg --packagelist` stdout: one artifact path per line. Blank
/// lines are dropped and surrounding whitespace trimmed; every other line is
/// taken verbatim as a path (makepkg emits absolute `PKGDEST/…` paths).
fn parse_package_list(stdout: &str) -> Vec<PathBuf> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Forward SIGINT to makepkg's whole process group so its build children
/// (make, cc, …) stop too, not just makepkg itself. makepkg runs under the pty
/// in its own session (`setsid`), so its pgid equals its pid.
fn forward_sigint(pid: Option<u32>) {
    let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) else {
        warn!("no makepkg pid to forward SIGINT to");
        return;
    };
    if let Err(e) = killpg(Pid::from_raw(pid), Signal::SIGINT) {
        warn!(error = %e, "failed to forward SIGINT to makepkg process group");
    }
}

/// Pump bytes from the pty master to the user's terminal and the build log.
/// Read in raw chunks rather than by line so ANSI colour sequences and
/// `\r`-terminated progress bars (curl, wget) reach the terminal byte-for-byte.
///
/// Per-destination "log once, keep draining" on failure: a broken stdout or a
/// full disk on `build.log` shouldn't abort the build (and we must keep
/// reading the pty so makepkg doesn't block on a full master buffer), but the
/// user needs to know an output sink went away — surface via `tracing` (which
/// writes to aurox's own stderr/log, not the pty), once per sink.
fn tee<R: Read, W: Write>(mut reader: R, mut writer: W, log: &Mutex<File>) {
    let mut buf = [0u8; 8192];
    let mut stdout_failed = false;
    let mut log_failed = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => return,
            Err(e) => {
                warn!(error = %e, "pty read failed; build output truncated");
                return;
            }
            Ok(n) => {
                let slice = &buf[..n];
                if !stdout_failed
                    && let Err(e) = writer.write_all(slice).and_then(|()| writer.flush())
                {
                    warn!(error = %e, "stdout write failed; terminal mirror disabled for the remainder of the build");
                    stdout_failed = true;
                }
                if !log_failed
                    && let Ok(mut f) = log.lock()
                    && let Err(e) = f.write_all(slice)
                {
                    warn!(error = %e, "build.log write failed; log will be truncated");
                    log_failed = true;
                }
            }
        }
    }
}

/// Match the parent terminal size so progress bars and `==>` separators
/// wrap the way the user expects. Falls back to a sensible default when
/// stdout isn't a terminal (e.g. output redirected to a file).
fn pty_size() -> PtySize {
    let (rows, cols) = Term::stdout().size();
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_package_list_trims_and_skips_blanks() {
        let out = "/pkgs/foo-1.0-1-x86_64.pkg.tar.zst\n\n  /pkgs/bar-2.0-1-any.pkg.tar.zst  \n";
        assert_eq!(
            parse_package_list(out),
            vec![
                PathBuf::from("/pkgs/foo-1.0-1-x86_64.pkg.tar.zst"),
                PathBuf::from("/pkgs/bar-2.0-1-any.pkg.tar.zst"),
            ],
        );
    }

    /// The VCS case this whole freeze exists for: `makepkg --packagelist`
    /// reports the *dynamic* `pkgver()` result, so the frozen filename carries
    /// the built `r70.g0dae0f47c-1` — not the static `r2.g3e316c1c5-2` from
    /// `.SRCINFO`. Captured verbatim from a `selinux-refpolicy-arch-git` run.
    #[test]
    fn parse_package_list_captures_vcs_dynamic_pkgver() {
        let out = "/pkgs/selinux-refpolicy-arch-git-RELEASE_2_20260312.r70.g0dae0f47c-1-any.pkg.tar.zst\n";
        let list = parse_package_list(out);
        assert_eq!(list.len(), 1);
        assert!(
            list[0].to_string_lossy().contains("r70.g0dae0f47c-1"),
            "frozen filename must carry the dynamic pkgver, not the static one",
        );
    }
}
