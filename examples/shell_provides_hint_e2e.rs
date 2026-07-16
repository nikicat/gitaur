//! End-to-end driver for the shell upgrade's provides-hint plumbing, used by
//! `tests/container/extended/28_shell_upgrade_provides_hint.sh` (ports the
//! retired smoke/33, which drove the removed `-Syu` picker path).
//!
//! The dotnet-runtime shape: two foreign virtuals installed, both declared as
//! `provides=` by AUR pkgbase test-syu-hint-new (newer declared first) —
//!   * test-syu-hint-newer at 9.0, vercmp-newer than the pkgbase's 2.0, so it
//!     must NOT become an upgrade candidate;
//!   * test-syu-hint-older at 1.0, outdated, so `upgrade` seeds it — named by
//!     the *foreign pkgname*, which is the hint.
//!
//! The observable hint assertion is the review header: hinted, it labels the
//! transition `[provides test-syu-hint-older]` (the row the user acted on);
//! unhinted, the walk lands on the first-declared installed provides and the
//! header shows `[provides test-syu-hint-newer]` — the original wrong-label
//! bug. Then the review approves, `apply` builds + installs, and the `.sh`
//! asserts the pkgbase landed while the vercmp-newer foreign stayed untouched.

use pty_harness::{Pty, has};

fn main() {
    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    // Glob-seed both hint fixtures: only the outdated one is a candidate, so
    // exactly one row stages. The screen holds just the banner + this table,
    // so a bare name-absence check is safe here (later screens carry the
    // PKGBUILD text, whose provides array names -newer legitimately).
    pty.send(b"upgrade test-syu-hint-*\r");
    pty.expect("older staged by its foreign pkgname", |s| {
        has(s, "test-syu-hint-older 1.0-1 → 2.0-1")
    });
    let screen = pty.screen();
    assert!(
        !screen.contains("test-syu-hint-newer"),
        "vercmp-newer foreign must not seed an upgrade row\n--- screen ---\n{screen}\n--- end ---"
    );

    // The review header must carry the hinted counterpart.
    pty.send(b"review test-syu-hint-older\r");
    pty.expect("hinted provides annotation", |s| {
        has(s, "[provides test-syu-hint-older]")
    });
    let screen = pty.screen();
    assert!(
        !has(&screen, "[provides test-syu-hint-newer]"),
        "review header shows the first-declared provides — the hint didn't reach \
         the counterpart walk\n--- screen ---\n{screen}\n--- end ---"
    );
    pty.expect("review prompt", |s| s.contains("(y)es"));
    pty.send(b"y\r");
    pty.expect("approved via review", |s| {
        s.contains("approved test-syu-hint-older")
    });

    pty.send(b"apply\r");
    pty.expect("sudo gate", |s| s.contains("Continue?"));
    pty.send(b"\r");
    // The built pkgname (the pkgbase) differs from the user's target spec
    // (the foreign pkgname), so the install-reason pass doesn't recognise it
    // as a direct target and demotes it via a second elevated
    // `pacman -D --asdeps` — its gate needs an answer too.
    pty.expect("asdeps reason gate", |s| s.contains("pacman -D --asdeps"));
    pty.send(b"\r");
    pty.expect("apply finished", |s| s.contains("done"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_PROVIDES_HINT_E2E_OK");
}
