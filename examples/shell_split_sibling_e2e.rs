//! End-to-end driver for the shell upgrade of a foreign split sibling, used by
//! `tests/container/extended/29_shell_upgrade_split_sibling.sh` (ports the
//! retired smoke/44, the google-cloud-cli regression, onto the shell path).
//!
//! test-syu-split-foreign-cli is installed foreign at 1.0; its pkgbase
//! test-syu-split-foreign (2.0) also packages -daemon and -desktop. The shell
//! `upgrade` seeds the row named by the foreign pkgname, whose hint must ride
//! through resolve → `Plan.pkgname_selections` → the install filter so `apply`
//! lands ONLY the picked sibling — the `.sh` asserts -daemon/-desktop stayed
//! out of localdb (the original bug installed every sibling makepkg produced).

use pty_harness::{Pty, has};

fn main() {
    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    pty.send(b"upgrade test-syu-split-foreign-cli\r");
    pty.expect("foreign sibling staged", |s| {
        has(s, "test-syu-split-foreign-cli 1.0-1 → 2.0-1")
    });

    pty.send(b"approve *\r");
    pty.expect("approved", |s| {
        s.contains("approved test-syu-split-foreign-cli")
    });

    pty.send(b"apply\r");
    pty.expect("sudo gate", |s| s.contains("Continue?"));
    pty.send(b"\r");
    pty.expect("apply finished", |s| s.contains("done"));

    pty.send(b"show\r");
    pty.expect("cart cleared after apply", |s| s.contains("cart is empty"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_SPLIT_SIBLING_E2E_OK");
}
