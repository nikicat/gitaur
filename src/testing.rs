//! Shared test helpers used by both unit tests (in `src/`) and integration
//! tests (in `tests/`). Not part of the public API.
//!
//! `#[cfg(test)]` would gate this out of integration tests (which see the lib
//! built without `--test`), so the module is unconditional `pub` with
//! `#[doc(hidden)]` to keep it out of generated docs.

use crate::paths::STATE_ROOT_OVERRIDE;
use std::path::{Path, PathBuf};
use std::process::Command;

/// RAII guard that reroutes [`crate::paths::state_dir`] (and everything that
/// derives from it — `aur_repo_path`, `index_path`, `pkg_worktree`, …) to a
/// custom root for the lifetime of the guard. Restores the previous value
/// on drop, including the panic path.
///
/// Backed by thread-local storage, so two tests on different runner threads
/// can install independent overrides without colliding. Concurrent code
/// spawned *within* a guarded scope (rayon workers, std threads) does NOT
/// inherit the override — keep `paths::*` calls on the test thread.
pub struct ScopedStateRoot {
    previous: Option<PathBuf>,
}

impl ScopedStateRoot {
    /// Install `root` as the active state root on the current thread.
    pub fn new(root: PathBuf) -> Self {
        let previous = STATE_ROOT_OVERRIDE.with(|c| c.borrow_mut().replace(root));
        Self { previous }
    }
}

impl Drop for ScopedStateRoot {
    fn drop(&mut self) {
        STATE_ROOT_OVERRIDE.with(|c| {
            *c.borrow_mut() = self.previous.take();
        });
    }
}

/// Run `git <args>` in `cwd` with a clean, deterministic identity and no host
/// config — bypasses things like the developer's `commit.gpgsign=true` that
/// would otherwise turn fixture commits into interactive GPG prompts.
///
/// Panics on non-zero exit; tests only.
pub fn git(args: &[&str], cwd: &Path) {
    let status = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git available");
    assert!(status.success(), "git {args:?} failed in {}", cwd.display());
}

/// Capture stdout of `git <args>` in `cwd`, with the same clean env as [`git`].
pub fn git_stdout(args: &[&str], cwd: &Path) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git available");
    assert!(
        out.status.success(),
        "git {args:?} failed in {}",
        cwd.display()
    );
    String::from_utf8(out.stdout).expect("git stdout utf8")
}
