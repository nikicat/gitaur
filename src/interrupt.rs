//! A scoped SIGINT guard for long-running, cooperatively-cancellable work.
//!
//! Inside the shell, a Ctrl+C during a multi-second operation must abort *that
//! operation* and land back at a live prompt — but without a signal handler
//! installed it hits the kernel's default SIGINT disposition and terminates the
//! whole process. [`cancel_on_sigint`] closes that gap for any operation that
//! can watch a flag: the AUR mirror fetch/clone (gix polls the flag
//! cooperatively, and the curl transport's transfer meter aborts a read parked
//! on an idle socket — see `mirror::http_transport_options`) and the official
//! repo-DB refresh (`pacman::dload` aborts its transfer from curl's progress
//! callback).
//!
//! Multiple guards may be live at once — the parallel halves of a refresh each
//! hold their own. `signal_hook` fans one SIGINT out to *every* registered
//! action, so each half sees the same Ctrl+C and unwinds independently.

use crate::context;
use crate::error::{Error, Result};
use signal_hook::consts::SIGINT;
use signal_hook::iterator::Signals;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Run a blocking operation under a scoped SIGINT guard, so a terminal Ctrl+C
/// aborts *the operation* instead of killing aurox.
///
/// Installs a scoped handler that suppresses the default die-on-SIGINT for the
/// operation's duration, runs a watcher thread that blocks on the signal pipe
/// (no polling) and flips the `interrupt` flag on each Ctrl+C, and lets `run`
/// unwind cooperatively — `run` is responsible for actually watching the flag
/// (or handing it to machinery that does). The RAII drop of `Signals` restores
/// the previous disposition once no guard remains. On interrupt this returns
/// [`Error::Interrupted`] regardless of what `run` returned — the raw error an
/// aborted transfer surfaces is an artifact of the abort, not the story.
pub fn cancel_on_sigint<T>(run: impl FnOnce(&Arc<AtomicBool>) -> Result<T>) -> Result<T> {
    let interrupt = Arc::new(AtomicBool::new(false));
    let mut signals = Signals::new([SIGINT])?;
    let handle = signals.handle();
    let outcome = context::scope(|s| {
        // Watcher: blocks on the signal pipe and flips the flag on each Ctrl+C;
        // `handle.close()` ends it once `run` returns (whether the operation
        // completed or unwound).
        s.spawn(|| {
            for _ in &mut signals {
                interrupt.store(true, Ordering::SeqCst);
            }
        });
        let outcome = run(&interrupt);
        handle.close();
        outcome
    });
    if interrupt.load(Ordering::SeqCst) {
        return Err(Error::Interrupted);
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    // The watcher-thread → real-signal path is covered end-to-end by the
    // container suite (a PTY sends a real Ctrl+C mid-fetch), not here: raising
    // SIGINT in-process would fire *every* live `Signals` instance's action —
    // the single OS handler runs them all — and so corrupt any other
    // `cancel_on_sigint` running in parallel under `cargo test`. These tests
    // stand in for the watcher by flipping the cooperative flag directly, which
    // exercises the guard's own logic (scope/teardown + flag→outcome mapping)
    // with no signal in flight.

    /// Happy path: `run` finishes without an interrupt, so its value passes
    /// through untouched and the guard tears down cleanly.
    #[test]
    fn returns_the_run_value_when_uninterrupted() {
        let out = cancel_on_sigint(|_interrupt| Ok(42u32));
        assert!(matches!(out, Ok(42)));
    }

    /// A non-interrupt error propagates verbatim — the guard rewrites the
    /// outcome only when its flag was actually flipped.
    #[test]
    fn passes_through_a_non_interrupt_error() {
        let out: Result<()> = cancel_on_sigint(|_interrupt| Err(Error::other("boom")));
        assert!(matches!(out, Err(Error::Other(msg)) if msg == "boom"));
    }

    /// Once the cooperative flag is flipped — as the watcher does on Ctrl+C —
    /// the outcome is normalized to `Error::Interrupted`, replacing whatever
    /// error the aborted operation surfaced when it noticed the flag and
    /// unwound.
    #[test]
    fn maps_a_flipped_flag_to_interrupted() {
        let out: Result<()> = cancel_on_sigint(|interrupt| {
            interrupt.store(true, Ordering::SeqCst);
            Err(Error::gix("receive", std::io::Error::other("cancelled")))
        });
        assert!(matches!(out, Err(Error::Interrupted)));
    }

    /// The flag is authoritative even if `run` happened to finish `Ok` in the
    /// same instant the interrupt landed — mirrors the build path, which treats
    /// a set flag as interrupted regardless of makepkg's exit status.
    #[test]
    fn prefers_interrupted_over_a_racing_ok() {
        let out = cancel_on_sigint(|interrupt| {
            interrupt.store(true, Ordering::SeqCst);
            Ok(7u32)
        });
        assert!(matches!(out, Err(Error::Interrupted)));
    }
}
