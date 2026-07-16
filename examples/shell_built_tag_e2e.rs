//! End-to-end driver for the shell upgrade table's already-built column, used
//! by `tests/container/extended/27_shell_upgrade_aur_built_tag.sh` (ports the
//! retired `06_loop_built_tag` now that the picker is gone).
//!
//! The `.sh` stages an installed-but-outdated foreign AUR package whose
//! *new*-version artifact already sits in the build worktree — the leftover of
//! a build that completed earlier but wasn't installed. The shell's `upgrade`
//! stages the candidate and its unified transaction table must flag the row
//! `built` (the read-only mirror of `prepare_one`'s idempotency check), proving
//! worktree path + artifact filename + index version line up end to end. The
//! render is the whole assertion — no apply.

use pty_harness::{Pty, has};

fn main() {
    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    // Seed only the fixture row — a bare `upgrade` would also stage whatever
    // real core/extra upgrades the image carries. The `.sh` ran `-Sy` moments
    // ago, so the TTL check skips the fetch and the staged row renders from
    // the fresh index.
    pty.send(b"upgrade test-trivial\r");
    pty.expect("staged upgrade row", |s| {
        has(s, "test-trivial 1.0-1 → 2.0-1")
    });
    pty.expect("built tag on the pre-built row", |s| s.contains("built"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_BUILT_TAG_E2E_OK");
}
