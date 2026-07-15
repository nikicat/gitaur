//! End-to-end driver for the shell's first-launch AUR question, the "Later"
//! default — used by `tests/container/extended/12_shell_first_launch.sh`.
//!
//! ```text
//!   (launch)  → the three-way question; nothing clones on its own
//!   Enter     → "Later": banner + the pacman-only-this-session reminder
//!   refresh   → pre-consented: brief heads-up, bootstrap, index refreshed
//!   search    → an AUR row resolves in the same session (data reloaded)
//!   quit      → clean exit
//! ```
//!
//! The `.sh` asserts the flip side: "Later" persisted nothing to config.toml.

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_aurox();

    // The question is a plain line read (pre-rustyline), so Enter answers it
    // with the default: Later.
    pty.expect("three-way question", |s| s.contains("sync the AUR now?"));
    pty.send(b"\r");
    pty.expect("later reminder", |s| s.contains("pacman-only this session"));

    // `refresh` after the launch question IS the consent — a one-line
    // heads-up instead of a second Y/n, then the bootstrap runs.
    pty.send(b"refresh\r");
    pty.expect("pre-consented heads-up", |s| {
        s.contains("syncing the AUR — one-time")
    });
    pty.expect("refresh outcome", |s| {
        s.contains("mirror + index refreshed")
    });

    // The session reloaded the fresh index: an AUR-only name resolves now.
    // Match the result row (repo bucket + name on one line), not the echo of
    // the typed command — that line contains "search", the row doesn't.
    pty.send(b"search ^test-trivial$\r");
    pty.expect("aur row", |s| {
        s.lines()
            .any(|l| l.contains("test-trivial") && l.contains("aur") && !l.contains("search"))
    });

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_BOOTSTRAP_LATER_E2E_OK");
}
