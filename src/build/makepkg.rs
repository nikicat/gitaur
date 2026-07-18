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
use nix::sys::termios::{LocalFlags, tcgetattr};
use nix::unistd::Pid;
use portable_pty::{Child, CommandBuilder, ExitStatus, PtySize, native_pty_system};
use signal_hook::consts::SIGINT;
use signal_hook::iterator::Signals;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::Duration;
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
/// Returns the path to the captured `build.log` on success; on failure the
/// same path is embedded in the [`Error::Build`] message.
#[instrument(skip(cfg))]
pub fn run(
    cfg: &Config,
    worktree: &Path,
    extra_args: &[&str],
    log_mode: LogMode,
) -> Result<PathBuf> {
    let log_path = worktree.join("build.log");
    let log_file = open_log(&log_path, log_mode)?;
    let SpawnedBuild {
        mut child,
        reader,
        group,
    } = spawn_under_pty(cfg, worktree, extra_args, &log_path)?;
    trace_terminal_state();

    // Catch SIGINT for the duration of this build. `Signals` installs an
    // async-signal-safe handler that suppresses the default action (which would
    // kill aurox) and feeds a blocking iterator instead — so a Ctrl+C mid-build
    // unwinds as `Error::Interrupted` and the no-arg loop bails back to the
    // table rather than the whole program dying. Dropping `Signals` only
    // unregisters *this action*: signal-hook's process-wide handler stays
    // installed for the life of the process (signal-hook-registry's
    // `unregister` never restores the OS disposition, and its dispatcher
    // chains to the pre-existing handler only when that was a custom one —
    // SIG_DFL is skipped). So after the first build, a SIGINT arriving while
    // no guard is live is *swallowed*, not fatal — every post-build Ctrl+C
    // surface needs its own handling (the shell reads ^C as a byte in
    // rustyline's raw mode; the `-S` path runs to completion and exits).
    let mut signals = Signals::new([SIGINT])?;
    let handle = signals.handle();
    let interrupted = AtomicBool::new(false);
    // Closed (by dropping the sender) the moment `child.wait()` returns —
    // [`forward_until_exit`]'s verify loop blocks on it, not on a poll.
    let (exited_tx, exited_rx) = mpsc::channel::<()>();

    let log = Mutex::new(log_file);
    let status = context::scope(|s| -> Result<ExitStatus> {
        s.spawn(|| tee(reader, std::io::stdout(), &log));
        // `exited_rx` is `!Sync`, so the watcher takes it by move; the
        // signals iterator and flag ride along as pre-made borrows.
        let signals_iter = &mut signals;
        let interrupted_ref = &interrupted;
        s.spawn(move || forward_until_exit(signals_iter, interrupted_ref, exited_rx, group));
        // No `?` before the drop: the sender must fall (unblocking the
        // watcher's verify loop) and the handle must close on the error path
        // too, or the scope's join would hang on a live watcher.
        let status = child
            .wait()
            .map_err(|e| Error::Build(format!("wait makepkg: {e}")));
        drop(exited_tx);
        handle.close();
        status
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

/// How a makepkg pass treats the worktree's `build.log` — named so a call
/// site reads as intent, not as an opaque bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogMode {
    /// Truncate: this is the run's first makepkg pass.
    Fresh,
    /// Append: a later pass of the same run (the VCS `--nobuild` then
    /// `--noextract` sequence), so a multi-pass build lands in one
    /// contiguous log.
    Append,
}

/// Open `build.log` in the worktree per [`LogMode`].
fn open_log(log_path: &Path, log_mode: LogMode) -> Result<File> {
    Ok(match log_mode {
        LogMode::Fresh => File::create(log_path)?,
        LogMode::Append => OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?,
    })
}

/// A makepkg child freshly spawned under its own pty — the handles [`run`]
/// needs: the child to wait on, the master-side reader the tee pumps, and
/// the process group the interrupt watcher forwards to.
struct SpawnedBuild {
    child: Box<dyn Child + Send + Sync>,
    reader: Box<dyn Read + Send>,
    group: BuildGroup,
}

/// Open the pty, assemble the makepkg command (worktree-pinned build dirs,
/// configured + extra args), and spawn. The slave side is dropped here so
/// master reads see EOF the instant the child (the only other holder of the
/// slave fds) exits.
fn spawn_under_pty(
    cfg: &Config,
    worktree: &Path,
    extra_args: &[&str],
    log_path: &Path,
) -> Result<SpawnedBuild> {
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

    let child = pty
        .slave
        .spawn_command(cmd)
        .map_err(|e| Error::Build(format!("spawn makepkg: {e}")))?;
    drop(pty.slave);
    let group = BuildGroup::new(child.process_id());
    let reader = pty
        .master
        .try_clone_reader()
        .map_err(|e| Error::Build(format!("pty reader: {e}")))?;
    Ok(SpawnedBuild {
        child,
        reader,
        group,
    })
}

/// The interrupt path's preconditions, recorded because issue #59's failing
/// runs were undiagnosable without them: a terminal `^C` raises SIGINT only
/// while stdin's ISIG is set — with ISIG off, ECHOCTL still paints a
/// misleading `^C` on screen while the byte just sits in the input queue.
/// One debug line per build makes a captured failure self-explaining.
fn trace_terminal_state() {
    match tcgetattr(std::io::stdin()) {
        Ok(t) => {
            let l = t.local_flags;
            debug!(
                isig = l.contains(LocalFlags::ISIG),
                icanon = l.contains(LocalFlags::ICANON),
                echo = l.contains(LocalFlags::ECHO),
                "terminal state at build start"
            );
        }
        Err(e) => debug!(error = %e, "no stdin termios at build start"),
    }
}

/// The interrupt watcher: block on the signal pipe (no polling); on Ctrl+C
/// note the interrupt and forward SIGINT to makepkg's process group — then
/// *verify delivery*. A group-directed SIGINT can be consumed with no
/// effect when it lands inside the fork→exec window of a child that
/// makepkg (bash, INT-trapped) is just spawning: the pre-exec child holds
/// the parent's trap disposition, whose handler flag `execve` then
/// discards, so e.g. `sleep` starts unsignalled and bash defers its trap
/// until that child exits — never. Proven by
/// `examples/sigint_forward_stress --trap` (~5% of rounds hang on an idle
/// host; `--verified`, this exact loop, 0/1000); issue #59's captured
/// failure showed one successful killpg with a live build 20 s later.
/// Re-forwarding until makepkg actually exits (`exited`'s sender drops)
/// turns a swallowed forward into a one-beat delay; the group is already
/// condemned, so repeats are harmless.
///
/// A single blocking take: the first Ctrl+C is handled to completion, so
/// there is never a second pass — further Ctrl+Cs would only re-signal an
/// already-condemned group. The caller ends a signal-less watch via
/// [`signal_hook::iterator::Handle::close`], and must drop `exited`'s
/// sender once the child is reaped — on error paths too — or the scope's
/// join would hang on the verify loop.
// The receiver is deliberately owned: `mpsc::Receiver` is `!Sync`, so the
// spawned watcher thread cannot borrow it from the spawning scope.
#[expect(clippy::needless_pass_by_value)]
fn forward_until_exit(
    signals: &mut Signals,
    interrupted: &AtomicBool,
    exited: mpsc::Receiver<()>,
    group: BuildGroup,
) {
    if let Some(sig) = signals.forever().next() {
        interrupted.store(true, Ordering::SeqCst);
        debug!(sig, "interrupt watcher woke; forwarding");
        group.forward_sigint();
        // Each timeout means the build is still alive past the grace window
        // — the forward was eaten by the fork-window race, so do it again.
        // Anything else is the sender dropping: `child.wait()` returned,
        // delivery verified.
        while exited.recv_timeout(Duration::from_millis(300))
            == Err(mpsc::RecvTimeoutError::Timeout)
        {
            debug!("build alive after SIGINT forward; re-forwarding");
            group.forward_sigint();
        }
    }
    debug!("interrupt watcher closed");
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

/// The interrupt-forward target: makepkg's process group. portable-pty
/// `setsid`s the child, so its pid *is* its pgid — that fact and the raw-pid
/// conversion are captured once here at spawn, instead of an `Option<u32>`
/// caravan re-deriving them at every forward site. Empty when the backend
/// reported no pid (a forward then warns instead of guessing).
#[derive(Debug, Clone, Copy)]
struct BuildGroup(Option<Pid>);

impl BuildGroup {
    /// From portable-pty's `process_id` answer.
    fn new(pid: Option<u32>) -> Self {
        Self(pid.and_then(|p| i32::try_from(p).ok()).map(Pid::from_raw))
    }

    /// Forward SIGINT to the whole group so makepkg's build children (make,
    /// cc, …) stop too, not just makepkg itself. Failures are warnings — a
    /// lost forward must never turn an interrupt into a crash (and the
    /// verify loop retries it anyway).
    fn forward_sigint(self) {
        let Some(pgid) = self.0 else {
            warn!("no makepkg pid to forward SIGINT to");
            return;
        };
        match killpg(pgid, Signal::SIGINT) {
            Ok(()) => {
                debug!(
                    pgid = pgid.as_raw(),
                    "forwarded SIGINT to makepkg process group"
                );
            }
            Err(e) => {
                warn!(error = %e, pgid = pgid.as_raw(), "failed to forward SIGINT to makepkg process group");
            }
        }
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
