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
//! Backed by `thread_local!` — every `exec_pacman` call in this crate runs
//! on the main thread (the only `rayon::join` site is index loading, which
//! never spawns pacman), so a TLS is the right granularity. The pattern
//! mirrors [`crate::paths::STATE_ROOT_OVERRIDE`] / [`crate::testing::ScopedStateRoot`].

use std::cell::Cell;

/// Snapshot of clap-derived flags that need to be visible deep in the call tree.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunOpts {
    /// User passed `--noconfirm`: skip every interactive gitaur prompt
    /// (matches pacman's flag of the same name; gitaur threads it into
    /// its own prompts including the pre-sudo confirmation).
    pub noconfirm: bool,
}

thread_local! {
    static RUN_OPTS: Cell<RunOpts> = const { Cell::new(RunOpts { noconfirm: false }) };
}

/// Install `opts` for the current thread. Last writer wins; there's no stack.
pub fn set(opts: RunOpts) {
    RUN_OPTS.with(|c| c.set(opts));
}

/// Snapshot of the active options for the current thread.
pub fn get() -> RunOpts {
    RUN_OPTS.with(Cell::get)
}

/// `--noconfirm` shorthand — the only field most callers care about.
pub fn noconfirm() -> bool {
    get().noconfirm
}

/// True iff `argv` contains a `--noconfirm` token. Used by the pre-clap
/// pass-through path in [`crate::cli::run`], where `Cli::parse` never runs.
pub fn argv_has_noconfirm(argv: &[String]) -> bool {
    argv.iter().any(|a| a == "--noconfirm")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set` overwrites; `get` returns the latest value. No stacking — callers
    /// that need stacking are expected to snapshot+restore manually.
    #[test]
    fn set_and_get_roundtrip() {
        set(RunOpts { noconfirm: true });
        assert!(noconfirm());
        set(RunOpts { noconfirm: false });
        assert!(!noconfirm());
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
}
