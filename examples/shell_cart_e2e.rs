//! End-to-end driver for the interactive shell's cart → approve → apply flow
//! (REPL phase 3), used by `tests/container/extended/07_shell_cart_apply.sh`.
//!
//! The shell only runs interactively (stdin must be a TTY), so this spawns the
//! real no-arg `aurox` under a PTY (via the shared [`pty_harness::Pty`]) and
//! scripts the staged-transaction flow against the `test-trivial` AUR fixture:
//!
//! ```text
//!   add test-trivial        → staged, needs review (ack + status line)
//!   apply                   → refused: the approval gate blocks it
//!   approve test-trivial    → cleared without opening a diff
//!   apply                   → cost summary, build, then the sudo gate
//!   show                    → "cart is empty" — the clean apply emptied it
//! ```
//!
//! Each step both drives the shell and asserts the line it should print. The
//! `.sh` runs `aurox -Sy` first so the shell's on-disk index can classify
//! `test-trivial` as AUR (the shell does not fetch at startup), and asserts the
//! package is actually installed once this driver exits clean.

use pty_harness::Pty;

fn main() {
    let mut pty = Pty::spawn_aurox();

    // The shell starts at its prompt; the index was built by the `.sh`'s `-Sy`.
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    // Stage the AUR fixture — it lands needing review (review_default=prompt).
    pty.send(b"add test-trivial\r");
    pty.expect("staged test-trivial", |s| s.contains("staged test-trivial"));

    // The approval gate refuses to apply while the AUR item is unreviewed.
    pty.send(b"apply\r");
    pty.expect("apply gated on review", |s| s.contains("needs review"));

    // Approve without opening a diff, then apply for real.
    pty.send(b"approve test-trivial\r");
    pty.expect("approved", |s| s.contains("approved test-trivial"));

    pty.send(b"apply\r");
    // The explicit `apply` is the consent — no transaction confirm. The
    // one-line cost summary prints and the build runs; no deps are pulled in
    // (only_requested), so the first and only prompt is the sudo gate before
    // the privileged `pacman -U`.
    pty.expect("sudo gate", |s| s.contains("Continue?"));
    pty.send(b"\r");

    // Wait for the apply to finish (build + privileged `pacman -U`) before
    // driving the next command: sending `show` mid-install races the install, and
    // the buffered input is dropped when rustyline re-enters raw mode at the next
    // prompt. A clean apply prints `done` and clears the cart.
    pty.expect("apply finished", |s| s.contains("done"));
    // `show` then reports the cart empty — the shell-side proof the build +
    // install succeeded (a failure would keep the cart and this would time out).
    pty.send(b"show\r");
    pty.expect("cart cleared after apply", |s| s.contains("cart is empty"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_CART_E2E_OK");
}
