//! End-to-end driver for the shell upgrade table's already-built column, used
//! by `tests/container/extended/27_shell_upgrade_aur_built_tag.sh` (ports the
//! retired `06_loop_built_tag` now that the picker is gone).
//!
//! The `.sh` sets up an installed-but-outdated AUR package whose *new*-version
//! `.pkg.tar.zst` already sits in its build directory — the leftover of a
//! build that finished earlier but was never installed. The shell's `upgrade`
//! stages the candidate, and its table must mark the row `built`: installing
//! would reuse that file instead of rebuilding. The tag comes from the same
//! already-built check the build pipeline uses, so the tag being right proves
//! the directory path, the artifact filename, and the indexed version all
//! agree. The render is the whole assertion — nothing is applied.

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
