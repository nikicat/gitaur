//! Spawn `makepkg` with deterministic PKGDEST/SRCDEST/BUILDDIR placement.
//!
//! makepkg's stdout/stderr is tee'd: live bytes still flow to the terminal so
//! the user sees the build progress, and a verbatim copy is captured to
//! `<worktree>/build.log` for post-mortem debugging when a build fails inside
//! a multi-pkgbase run.

use crate::config::Config;
use crate::error::{Error, Result};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

    let mut cmd = Command::new(&cfg.makepkg_path);
    cmd.current_dir(worktree)
        .env("PKGDEST", worktree)
        .env("SRCDEST", worktree.join("src-cache"))
        .env("BUILDDIR", worktree)
        .args(&cfg.makepkg_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    debug!(args = ?cfg.makepkg_args, cwd = %worktree.display(), log = %log_path.display(), "spawning makepkg");

    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");

    // Scoped threads so both pumps complete before the function returns and
    // the log file drop closes cleanly. No Arc: `&Mutex<File>` is enough
    // when we borrow it on the parent stack.
    let log = Mutex::new(log_file);
    thread::scope(|s| {
        s.spawn(|| tee(stdout, std::io::stdout(), &log));
        s.spawn(|| tee(stderr, std::io::stderr(), &log));
    });

    let status = child.wait()?;
    if !status.success() {
        let code = status.code().unwrap_or(1);
        return Err(Error::Build(format!(
            "makepkg exited with status {code} (log: {})",
            log_path.display(),
        )));
    }
    info!(log = %log_path.display(), "makepkg succeeded");
    Ok(log_path)
}

/// Pump bytes from `reader` to both `writer` (terminal) and the shared log
/// file. Read in raw chunks rather than by line so `\r`-terminated progress
/// bars (curl, wget) reach the terminal as makepkg writes them.
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
