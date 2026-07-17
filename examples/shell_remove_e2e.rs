//! End-to-end driver for the shell's `remove` verb, used by
//! `tests/container/extended/32_shell_remove_stages_uninstall.sh`.
//!
//! `remove <pkg>` stages an uninstall (not to be confused with `drop`, which
//! un-stages a cart row — extended/09 pins the refusal when the target is a
//! staged install). The quiet mutation prints a status line counting the
//! removal; `show` renders the transaction with its "will remove" block, and
//! `apply` runs the `pacman -R` lane behind the sudo gate. The `.sh`
//! installs test-trivial first and asserts it's gone afterwards.

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    pty.send(b"remove test-trivial\r");
    pty.expect("removal staged", |s| {
        s.contains("staged removal of test-trivial")
    });
    pty.expect("status counts the removal", |s| s.contains("1 to remove"));
    pty.send(b"show\r");
    pty.expect("will-remove block", |s| s.contains("will remove"));

    pty.send(b"apply\r");
    pty.expect("sudo gate for pacman -R", |s| s.contains("Continue?"));
    pty.send(b"\r");
    // The removal lane runs pacman interactively (the shell has no
    // --noconfirm), so pacman's own remove confirm follows the sudo gate.
    pty.expect("pacman remove confirm", |s| {
        s.contains("Do you want to remove these packages?")
    });
    pty.send(b"\r");
    pty.expect("apply finished", |s| s.contains("done"));

    pty.send(b"show\r");
    pty.expect("cart cleared after apply", |s| s.contains("cart is empty"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_REMOVE_E2E_OK");
}
