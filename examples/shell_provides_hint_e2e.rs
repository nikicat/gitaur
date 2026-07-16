//! End-to-end driver for a shell upgrade of a package the user knows by a
//! different name, used by
//! `tests/container/extended/28_shell_upgrade_provides_hint.sh` (ports the
//! retired smoke/33, which drove the removed `-Syu` picker path).
//!
//! Setup — the dotnet-runtime story: two installed packages exist in no repo
//! and have no AUR entry of their own ("foreign"). Their only upgrade path is
//! AUR package test-syu-hint-new, which lists both in `provides=`, the newer
//! one first:
//!   * test-syu-hint-newer, installed at 9.0 — newer than the AUR package's
//!     2.0, so no upgrade may be offered for it;
//!   * test-syu-hint-older, installed at 1.0 — outdated, so `upgrade` stages
//!     it, listed under that name (the name the user knows).
//!
//! `review` must then describe the upgrade in the same terms: its header
//! reads `[provides test-syu-hint-older]` — "this build stands in for the
//! test-syu-hint-older you have installed". The bug this guards against: the
//! lookup ignores which row the user acted on and just takes the first
//! installed name from the provides list, labelling the upgrade
//! `[provides test-syu-hint-newer]`. After approving, `apply` builds and
//! installs; the `.sh` checks the resulting system state.

use pty_harness::{Pty, has};

fn main() {
    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    // `upgrade` with a glob covering both fixtures: only the outdated one is
    // an upgrade candidate, so exactly one row is staged. At this point the
    // screen holds just the banner + this table, so checking that "-newer"
    // appears nowhere is safe (later screens print the PKGBUILD, whose
    // provides= array legitimately names it).
    pty.send(b"upgrade test-syu-hint-*\r");
    pty.expect("older staged by its foreign pkgname", |s| {
        has(s, "test-syu-hint-older 1.0-1 → 2.0-1")
    });
    let screen = pty.screen();
    assert!(
        !screen.contains("test-syu-hint-newer"),
        "no upgrade may be offered for the already-newer package\n--- screen ---\n{screen}\n--- end ---"
    );

    // The review header must name the package the user acted on.
    pty.send(b"review test-syu-hint-older\r");
    pty.expect("header names the user's package", |s| {
        has(s, "[provides test-syu-hint-older]")
    });
    let screen = pty.screen();
    assert!(
        !has(&screen, "[provides test-syu-hint-newer]"),
        "review header names the wrong installed package (the first one in \
         the provides list, not the one the user acted on)\n--- screen ---\n{screen}\n--- end ---"
    );
    pty.expect("review prompt", |s| s.contains("(y)es"));
    pty.send(b"y\r");
    pty.expect("approved via review", |s| {
        s.contains("approved test-syu-hint-older")
    });

    pty.send(b"apply\r");
    pty.expect("sudo gate", |s| s.contains("Continue?"));
    pty.send(b"\r");
    // Expect exactly one sudo prompt. The regression would add a second one:
    // the built name (test-syu-hint-new) doesn't match the typed name
    // (test-syu-hint-older), and without the hint check the package gets
    // mis-marked "installed as a dependency" via an elevated
    // `pacman -D --asdeps` — whose prompt would stall this expect.
    pty.expect("apply finished", |s| s.contains("done"));
    let screen = pty.screen();
    assert!(
        !screen.contains("pacman -D --asdeps"),
        "the package the user asked for (under its installed counterpart's \
         name) must stay explicitly installed, not be marked as a \
         dependency\n--- screen ---\n{screen}\n--- end ---"
    );

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_PROVIDES_HINT_E2E_OK");
}
