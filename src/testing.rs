//! Shared test helpers used by both unit tests (in `src/`) and integration
//! tests (in `tests/`). Not part of the public API.
//!
//! `#[cfg(test)]` would gate this out of integration tests (which see the lib
//! built without `--test`), so the module is unconditional `pub` with
//! `#[doc(hidden)]` to keep it out of generated docs.

use std::path::Path;
use std::process::Command;

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
