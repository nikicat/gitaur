//! End-to-end driver for the sysupgrade preflight (the libjpeg-turbo shape),
//! used by `tests/container/extended/11_sysupgrade_preflight.sh`.
//!
//! Staged state on entry (set up by the shell script): repo pkg
//! `test-jpeg-provider` installed at 1.0 with `provides=('test-libjpeg')`,
//! its local-repo copy at 2.0 *without* the provides, and a foreign
//! `test-breaks-dep` 1.0 whose `depends=('test-libjpeg')` that upgrade would
//! break. The AUR carries `test-breaks-dep` 2.0 depending on the concrete
//! provider instead — a rebuild fixes the breakage.
//!
//! The script walks every preflight surface:
//!   1. `upgrade` auto-stages both candidates → the preview renders the issue
//!      as *resolved by the staged rebuild* (no warning).
//!   2. `drop test-breaks-dep` → the preview now warns with the pacman-parity
//!      "breaks dependency" line and hints `add test-breaks-dep`.
//!   3. `apply` → the gate fires *before* the cost summary and sudo prompt;
//!      the default-no override declines on a bare Enter, cart kept.
//!   4. `add` + `approve` the rebuild → `apply` orders the blocker build +
//!      `pacman -U` ahead of the `pacman -Syu` lane, and the whole
//!      transaction lands.

use pty_harness::Pty;

/// Whitespace-insensitive containment: the preflight's warning/note lines run
/// past the 100-col PTY and wrap on the vt100 grid, splitting any long needle
/// with an injected row boundary. Compacting both sides makes the match
/// immune to where the wrap lands.
fn has(screen: &str, needle: &str) -> bool {
    let compact = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
    compact(screen).contains(&compact(needle))
}

fn main() {
    // The rebuild fix lives in the AUR, so this scenario needs the index:
    // answer the first-launch question with "y" — the SyncNow branch — and
    // let the mock-mirror bootstrap run before the banner. (The old flow
    // relied on `upgrade` bootstrapping silently; that trap is gone.)
    let mut pty = Pty::spawn_aurox();
    pty.expect("three-way question", |s| s.contains("sync the AUR now?"));
    pty.send(b"y\r");
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    // 1. Seed the upgrades — only the two fixture candidates (the container
    // image may carry real pending core/extra upgrades; staging those would
    // pull real downloads into the test). Both stage: the repo provider and
    // the AUR rebuild — so the preflight already sees its fix staged and
    // renders the informational note, not a blocking warning. (The unstaged
    // real candidates land in `--ignore`, which the preflight mirrors.)
    pty.send(b"upgrade test-jpeg-provider test-breaks-dep\r");
    pty.expect("repo upgrade staged", |s| {
        // Column padding varies with the widest row, so match compacted.
        has(s, "test-jpeg-provider 1.0-1 → 2.0-1")
    });
    pty.expect("issue resolved by the auto-staged rebuild", |s| {
        has(
            s,
            "breaks dependency 'test-libjpeg' required by test-breaks-dep",
        ) && has(s, "resolved by the staged rebuild of test-breaks-dep")
    });

    // 2. Un-stage the rebuild: the same issue must now surface as a warning
    // with the AUR-aware `add` remediation hint.
    pty.send(b"drop test-breaks-dep\r");
    pty.expect("preview warns with the add hint", |s| {
        has(s, "`add test-breaks-dep` stages a rebuild")
    });

    // 3. `apply` gates on the preflight before the cost summary / confirm /
    // sudo. Bare Enter takes the default — no — and the cart survives.
    pty.send(b"apply\r");
    pty.expect("override prompt", |s| {
        has(s, "Repo upgrade expected to fail")
    });
    assert!(
        !pty.screen().contains("about to elevate"),
        "sudo gate fired before the preflight gate\n--- screen ---\n{}\n--- end ---",
        pty.screen()
    );
    assert!(
        !pty.screen().contains("Proceed with this transaction"),
        "cost-summary confirm fired before the preflight gate\n--- screen ---\n{}\n--- end ---",
        pty.screen()
    );
    pty.send(b"\r");
    pty.expect("apply declined, cart kept", |s| {
        has(s, "apply cancelled — cart kept")
    });

    // 4. Follow the hint: stage + approve the rebuild, apply for real. Every
    // command waits for its acknowledgement — input sent while output is
    // still streaming is dropped when rustyline re-enters raw mode.
    pty.send(b"add test-breaks-dep\r");
    pty.expect("rebuild staged", |s| has(s, "staged test-breaks-dep"));
    pty.send(b"approve test-breaks-dep\r");
    pty.expect("rebuild approved", |s| has(s, "approved test-breaks-dep"));
    pty.send(b"apply\r");
    pty.expect("transaction confirm after the resolved note", |s| {
        s.contains("Proceed with this transaction")
    });
    pty.send(b"\r");

    // Blocker phase first: the rebuild's `pacman -U` must be elevated before
    // any `pacman -Syu` appears.
    pty.expect("blocker install sudo gate", |s| {
        s.contains("pacman -U") && s.contains("Continue?")
    });
    assert!(
        !pty.screen().contains("pacman -Syu"),
        "repo lane ran before the blocker rebuild installed\n--- screen ---\n{}\n--- end ---",
        pty.screen()
    );
    pty.send(b"\r");

    // Then the repo lane, now unblocked.
    pty.expect("repo lane sudo gate", |s| {
        s.contains("pacman -Syu") && s.contains("Continue?")
    });
    pty.send(b"\r");

    // Wait for the apply to finish before driving `show` (buffered input is
    // dropped when rustyline re-enters raw mode — see the PTY e2e notes).
    pty.expect("apply finished", |s| s.contains("done"));
    pty.send(b"show\r");
    pty.expect("cart cleared after apply", |s| s.contains("cart is empty"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_PREFLIGHT_E2E_OK");
}
