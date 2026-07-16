//! End-to-end driver for the shell's apply-failure resume, used by
//! `tests/container/extended/30_shell_apply_failure_keeps_cart.sh`.
//!
//! Two same-stratum AUR packages: test-trivial builds, test-fail-build's
//! `build()` returns 1. `apply` must isolate the failure (smoke/28's contract,
//! surfaced in the shell): the survivor still installs behind the sudo gate,
//! the fold drops the landed row and keeps ONLY the offender staged, and the
//! shell is back at a live prompt — `drop` the offender and the cart is empty,
//! no restart. The `.sh` asserts the end state in localdb.

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    pty.send(b"add test-fail-build\r");
    pty.expect("staged test-fail-build", |s| {
        s.contains("staged test-fail-build")
    });
    pty.send(b"add test-trivial\r");
    pty.expect("staged test-trivial", |s| s.contains("staged test-trivial"));

    pty.send(b"approve *\r");
    pty.expect("both approved", |s| {
        s.contains("approved test-fail-build") && s.contains("approved test-trivial")
    });

    pty.send(b"apply\r");
    // The stratum builds both: the failure is reported, then the survivor's
    // batched install fires the sudo gate.
    pty.expect("build failure reported", |s| {
        s.contains("test-fail-build: build failed")
    });
    pty.expect("sudo gate for the survivor", |s| s.contains("Continue?"));
    pty.send(b"\r");
    pty.expect("partial-failure fold", |s| {
        s.contains("apply partly failed") && s.contains("1 installed (dropped)")
    });

    // The offender is still staged; dropping it empties the cart (the drop
    // reprints the transaction, which is now empty).
    pty.send(b"drop test-fail-build\r");
    pty.expect("offender dropped", |s| {
        s.contains("dropped test-fail-build")
    });
    pty.expect("cart empty after drop", |s| s.contains("cart is empty"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_APPLY_FAILURE_E2E_OK");
}
