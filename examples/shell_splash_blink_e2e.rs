//! End-to-end for the launch splash's idle eye-blink: left untouched at the
//! first prompt, the ox winks "AUROX" in Morse — the eyes go `(oo)` → `(--)`
//! with no input at all. Driven under a PTY by
//! `tests/container/extended/37_shell_splash_blink.sh`.
//!
//! The shell only animates on an interactive terminal, so this spawns the real
//! no-arg `aurox` under a PTY (via [`pty_harness::Pty`]) and waits for the eyes
//! to blink shut. The complementary "a keystroke cancels the blink" path is
//! covered by every other shell driver: each types within the idle window and
//! finishes cleanly, which it couldn't if the wink had corrupted the line.

use pty_harness::{Pty, has};

fn main() {
    let mut pty = Pty::spawn_aurox();

    // The index was synced by the `.sh`, so the shell opens straight to the
    // banner (a never-synced AUR would ask the first-launch question ahead of
    // it instead).
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    // Sit idle. After the idle window the ox starts signalling, so the eyes
    // must close — the first Morse symbol ('A' = dit) shuts them to `(--)`,
    // which they never are while open (`(oo)`) or at rest.
    pty.expect("eyes wink shut", |s| has(s, "(--)"));

    // A keystroke ends both the blink and the session; the eyes reopen on the
    // way out and aurox exits clean.
    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_SPLASH_BLINK_E2E_OK");
}
