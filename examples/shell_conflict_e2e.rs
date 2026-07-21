//! End-to-end driver for the add-time declared-conflict reject, used by
//! `tests/container/extended/41_shell_conflict_reject.sh`.
//!
//! `test-xconflict-bin` declares `conflicts=('test-xconflict')` (no
//! `replaces=`). The shell resolves the *whole cart* at `add`, so staging both
//! must fail the conflict check up front and roll the cart back — the "a cart
//! with conflicting items is impossible" guarantee — rather than pacman's
//! prepare failing at apply, after the build:
//!
//! ```text
//!   add test-xconflict       → staged (aur)            ← resolves fine alone
//!   add test-xconflict-bin   → add rejected — … conflicts with … ; cart unchanged
//!   show                     → still just 1 to install (the base survived)
//! ```
//!
//! The `.sh` runs `aurox -Sy` first so the index carries both AUR entries, and
//! after this driver exits clean asserts neither package is installed (the
//! reject applied nothing).

use pty_harness::{Pty, has};

fn main() {
    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    // The base AUR package resolves + freezes fine on its own.
    pty.send(b"add test-xconflict\r");
    pty.expect("base staged from the AUR", |s| {
        has(s, "staged test-xconflict (aur)")
    });

    // The `-bin` declares `conflicts=test-xconflict`, which is co-staged. The
    // whole-cart resolve at `add` runs the conflict check and rejects — the
    // cart rolls back, so nothing new stages.
    pty.send(b"add test-xconflict-bin\r");
    pty.expect("conflict rejected at add", |s| {
        has(s, "add rejected") && has(s, "conflicts with")
    });

    // The reject preserved the existing cart: the base is still the only staged
    // install (a robust count check — the reject line still names the -bin, so
    // don't test for its absence as a substring).
    pty.send(b"show\r");
    pty.expect("only the base survived", |s| has(s, "1 to install"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_CONFLICT_E2E_OK");
}
