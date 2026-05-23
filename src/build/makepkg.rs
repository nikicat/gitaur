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
use crate::error::{Error, Result};
use console::Term;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use tracing::{debug, info, instrument};

/// Run `makepkg` in `worktree` with the configured args + env.
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
pub fn run(cfg: &Config, worktree: &Path) -> Result<PathBuf> {
    let log_path = worktree.join("build.log");
    let log_file = File::create(&log_path)?;

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
    debug!(args = ?cfg.makepkg_args, cwd = %worktree.display(), log = %log_path.display(), "spawning makepkg under pty");

    let mut child = pty
        .slave
        .spawn_command(cmd)
        .map_err(|e| Error::Build(format!("spawn makepkg: {e}")))?;
    // Drop our slave handle so master reads see EOF the instant the child
    // (the only other holder of the slave fds) exits.
    drop(pty.slave);

    let reader = pty
        .master
        .try_clone_reader()
        .map_err(|e| Error::Build(format!("pty reader: {e}")))?;

    let log = Mutex::new(log_file);
    thread::scope(|s| {
        s.spawn(|| tee(reader, std::io::stdout(), &log));
    });

    let status = child
        .wait()
        .map_err(|e| Error::Build(format!("wait makepkg: {e}")))?;
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

/// Pump bytes from the pty master to the user's terminal and the build log.
/// Read in raw chunks rather than by line so ANSI colour sequences and
/// `\r`-terminated progress bars (curl, wget) reach the terminal byte-for-byte.
fn tee<R: Read, W: Write>(mut reader: R, mut writer: W, log: &Mutex<File>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let slice = &buf[..n];
                let _ = writer.write_all(slice);
                let _ = writer.flush();
                if let Ok(mut f) = log.lock() {
                    let _ = f.write_all(slice);
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
