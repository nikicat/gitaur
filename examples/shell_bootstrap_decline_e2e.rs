//! End-to-end driver for the shell's first-launch AUR question, the "no,
//! pacman-only" answer — used by
//! `tests/container/extended/12_shell_first_launch.sh`.
//!
//! Two launches in sequence:
//!
//! ```text
//!   launch 1: question → n → "pacman-only mode saved" + (pacman-only) banner
//!   launch 2: no question, the (pacman-only) banner, no nag — the persisted
//!             `aur = false` sticks (the .sh asserts the config line itself)
//! ```

use pty_harness::Pty;

fn main() {
    // Launch 1: answer "n" — persisted and effective immediately (the banner
    // flips in the same session; the config handle updated disk + view).
    let mut pty = Pty::spawn_aurox();
    pty.expect("three-way question", |s| s.contains("sync the AUR now?"));
    pty.send(b"n\r");
    pty.expect("persisted note", |s| s.contains("pacman-only mode saved"));
    pty.expect("pacman-only banner", |s| {
        s.contains("aurox shell (pacman-only)")
    });
    pty.send(b"quit\r");
    pty.finish_clean();

    // Launch 2: the choice sticks — marked banner, no question, no nag.
    let mut pty = Pty::spawn_aurox();
    pty.expect("pacman-only banner", |s| {
        s.contains("aurox shell (pacman-only)")
    });
    let screen = pty.screen();
    assert!(
        !screen.contains("sync the AUR now?"),
        "second launch must not re-ask:\n{screen}"
    );
    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_BOOTSTRAP_DECLINE_E2E_OK");
}
