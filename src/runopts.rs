//! Per-invocation CLI options exposed via a thread-local.
//!
//! `--noconfirm` (and any future per-run flags like `--asdeps`) can appear
//! before or after the operation letter, get repeated by clap into deeply
//! nested call paths, and must be honored by code (e.g. `pacman::invoke`)
//! that has no other reason to know about clap. Threading the flags through
//! every function signature pollutes APIs that are otherwise about builds
//! or pacman dispatch; a thread-local installed once at the top of
//! [`crate::cli::run`] / [`crate::cli::dispatch::dispatch`] lets the leaves
//! read it directly.
//!
//! Backed by a [`context_local!`] so the flags reach code running on spawned /
//! rayon workers too (e.g. the AUR-index load in `build::cmd_install`'s
//! `context::join`, where `load_or_resync` must still see `--noresync`). The
//! pattern mirrors [`crate::paths::state_root`].

use crate::context_local;

/// Snapshot of clap-derived flags that need to be visible deep in the call tree.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOpts {
    /// User passed `--noconfirm`: skip every interactive gitaur prompt
    /// (matches pacman's flag of the same name; gitaur threads it into
    /// its own prompts including the pre-sudo confirmation).
    pub noconfirm: bool,

    /// User passed `--noresync`: don't auto-rebuild the AUR index when the
    /// on-disk archive is from an incompatible gitaur. Read by
    /// [`crate::index::load_or_resync`], which errors out instead of silently
    /// kicking off a network fetch + rebuild.
    pub noresync: bool,
}

context_local! {
    static opts: RunOpts = RunOpts { noconfirm: false, noresync: false };
}

/// Install `opts` for the current thread. Last writer wins; there's no stack.
pub fn set(o: RunOpts) {
    opts::set(o);
}

/// Snapshot of the active options for the current thread.
pub fn get() -> RunOpts {
    opts::get()
}

/// `--noconfirm` shorthand — the only field most callers care about.
pub fn noconfirm() -> bool {
    get().noconfirm
}

/// `--noresync` shorthand — read by [`crate::index::load_or_resync`].
pub fn noresync() -> bool {
    get().noresync
}

/// True iff `argv` contains a `--noconfirm` token. Used by the pre-clap
/// pass-through path in [`crate::cli::run`], where `Cli::parse` never runs.
pub fn argv_has_noconfirm(argv: &[String]) -> bool {
    argv_has_flag(argv, "--noconfirm")
}

/// True iff `argv` contains a `--noresync` token. Same pre-clap rationale as
/// [`argv_has_noconfirm`].
pub fn argv_has_noresync(argv: &[String]) -> bool {
    argv_has_flag(argv, "--noresync")
}

/// Exact-token match for a long flag — `--foo=bar` and substrings don't count.
fn argv_has_flag(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|a| a == flag)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set` overwrites; `get` returns the latest value. No stacking — callers
    /// that need stacking are expected to snapshot+restore manually.
    #[test]
    fn set_and_get_roundtrip() {
        set(RunOpts {
            noconfirm: true,
            noresync: true,
        });
        assert!(noconfirm());
        assert!(noresync());
        set(RunOpts::default());
        assert!(!noconfirm());
        assert!(!noresync());
    }

    #[test]
    fn argv_detection() {
        assert!(argv_has_noconfirm(&[
            "-S".into(),
            "--noconfirm".into(),
            "foo".into()
        ]));
        assert!(argv_has_noconfirm(&["--noconfirm".into()]));
        assert!(!argv_has_noconfirm(&["-S".into(), "foo".into()]));
        // Substring matches don't count — must be the exact token.
        assert!(!argv_has_noconfirm(&["--noconfirm=true".into()]));
    }

    #[test]
    fn noresync_argv_detection() {
        assert!(argv_has_noresync(&["-S".into(), "--noresync".into()]));
        assert!(!argv_has_noresync(&["-S".into(), "foo".into()]));
        // Exact-token only, same as --noconfirm.
        assert!(!argv_has_noresync(&["--noresync=1".into()]));
    }
}
