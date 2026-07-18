//! Probing variant of `shell_ctrl_c_e2e` for issue #59's failure farm.
//!
//! Same flow — stage `test-sleep-build`, apply, real `^C` once the build's
//! sentinel shows — but a lost interrupt is *interrogated*, not just
//! reported: after the classification line, a **second** `^C` goes down the
//! PTY. How that resolves discriminates the surviving hypotheses:
//!
//!   * `PROBE_SECOND_CTRL_C_RESCUED` — the machinery (guard, watcher,
//!     forward) is healthy and the *first* byte never became an effective
//!     interrupt: the fork-window swallow, one event wide.
//!   * `PROBE_SECOND_CTRL_C_DEAD`    — repeated `^C` does nothing: the
//!     guard/watcher is wedged or ISIG is off persistently; the aurox file
//!     log (termios line, watcher wakes, killpg outcomes) says which.
//!   * `PROBE_AUROX_EXITED`          — a `^C` killed aurox outright: the
//!     SIGINT guard wasn't installed at delivery time.
//!
//! Exit codes: 0 = interrupt landed normally; 1 = lost + rescued by the
//! second `^C`; 2 = lost + dead; 3 = aurox died. Non-zero always prints the
//! screen, so the farm's capture (plus the log tail extended/31-style)
//! carries the whole verdict.

// Exit codes are this probe's reporting protocol (0 ok / 1 rescued /
// 2 dead / 3 aurox died) — the farm keys on them.
#![allow(clippy::exit)]

use pty_harness::{Expectation, Pty};
use std::time::Duration;

fn main() {
    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    pty.send(b"add test-sleep-build\r");
    pty.expect("staged test-sleep-build", |s| {
        s.contains("staged test-sleep-build")
    });
    pty.send(b"approve *\r");
    pty.expect("approved", |s| s.contains("approved test-sleep-build"));

    pty.send(b"apply\r");
    pty.expect("build started (sentinel)", |s| {
        s.contains("AUROX_SLEEP_BUILD_SENTINEL")
    });

    // The user's Ctrl-C, as the terminal would deliver it.
    pty.send(&[0x03]);
    match pty.try_expect(Duration::from_secs(20), |s| s.contains("build interrupted")) {
        Expectation::Matched => {
            pty.expect("cart kept for retry", |s| {
                s.contains("apply failed — nothing installed; cart kept for retry")
            });
            pty.send(b"quit\r");
            pty.finish_clean();
            println!("SHELL_CTRL_C_PROBE_OK");
        }
        Expectation::Exited => died(&pty),
        Expectation::TimedOut => second_ctrl_c(&mut pty),
    }
}

/// The first `^C` was lost. Interrogate: does an identical second one land?
fn second_ctrl_c(pty: &mut Pty) -> ! {
    println!(
        "PROBE_FIRST_CTRL_C_LOST\n--- screen ---\n{}\n--- end ---",
        pty.screen()
    );
    pty.send(&[0x03]);
    match pty.try_expect(Duration::from_secs(10), |s| s.contains("build interrupted")) {
        Expectation::Matched => {
            println!("PROBE_SECOND_CTRL_C_RESCUED");
            std::process::exit(1);
        }
        Expectation::Exited => died(pty),
        Expectation::TimedOut => {
            println!(
                "PROBE_SECOND_CTRL_C_DEAD\n--- screen ---\n{}\n--- end ---",
                pty.screen()
            );
            std::process::exit(2);
        }
    }
}

/// A `^C` terminated aurox itself — the guard wasn't there to eat it.
fn died(pty: &Pty) -> ! {
    println!(
        "PROBE_AUROX_EXITED\n--- screen ---\n{}\n--- end ---",
        pty.screen()
    );
    std::process::exit(3);
}
