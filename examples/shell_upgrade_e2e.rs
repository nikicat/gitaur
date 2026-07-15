//! End-to-end driver for the shell's `upgrade` procedure (REPL phase 4), used
//! by `tests/container/extended/04_shell_upgrade_repo.sh`.
//!
//! No-arg `aurox` opens the shell. This drives the upgrade flow against an
//! installed-but-outdated **repo** package: `upgrade` refreshes + seeds the
//! pending upgrade (auto-approved, since it's a repo row), then `apply` renders
//! the cost-overlay change-set preview, takes the transaction confirm + the
//! sudo gate, and runs the partial `pacman -Syu`. A clean apply empties the
//! cart, which `show` confirms.
//!
//! It also folds in the synced-db **size guard** (the retired
//! `05_loop_size_from_synced_db` test): the preview total must be a real nonzero
//! figure, never `total  0 B` — the smoking gun of reading sizes from the stale
//! system syncdb (whose installed-version archive is cached → `0`) instead of
//! the freshly-synced db carrying the new version.

use pty_harness::Pty;

/// Whitespace-insensitive containment: the version columns pad to the widest
/// staged row and long lines wrap on the 100-col vt100 grid, so a literal
/// `1.0-1 → 2.0-1` match breaks whenever the container image carries real
/// pending upgrades with long version strings. Compacting both sides makes
/// the match immune to padding and wrap position.
fn has(screen: &str, needle: &str) -> bool {
    let compact = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
    compact(screen).contains(&compact(needle))
}

fn main() {
    // Never-synced state: the first-launch question comes before the banner.
    // Enter takes the default — Later — so this whole flow doubles as the
    // guard that a repo upgrade works without the AUR ever being set up.
    let mut pty = Pty::spawn_aurox();
    pty.expect("three-way question", |s| s.contains("sync the AUR now?"));
    pty.send(b"\r");
    pty.expect("shell banner", |s| s.contains("aurox shell"));

    // Refresh + seed the pending upgrade — only the fixture row. A bare
    // `upgrade` would also stage whatever real core/extra upgrades the image
    // happens to carry, turning apply into a multi-hundred-MiB real download;
    // the unstaged candidates land in `--ignore` instead, keeping the
    // `pacman -Syu` local to the fixture repo. The repo row auto-approves and
    // shows its old → new transition. With the AUR unsynced ("later"), the
    // upgrade must degrade to repo-only with a hint — never trigger the clone.
    pty.send(b"upgrade loop-repo\r");
    pty.expect("repo-only degradation note", |s| {
        has(s, "upgrades are repo-only")
    });
    pty.expect("repo upgrade staged", |s| has(s, "loop-repo 1.0-1 → 2.0-1"));

    // Apply gates on the one-line cost summary + a transaction confirm (phase
    // 5a folded the old apply-time change-set table into `show`/`upgrade`, so the
    // table — and its `this batch` total — now prints at `upgrade` above, not
    // here).
    pty.send(b"apply\r");
    pty.expect("transaction confirm", |s| {
        s.contains("Proceed with this transaction")
    });
    // The size guard from the retired `05_loop_size_from_synced_db`: the staged
    // upgrade's total (rendered by `upgrade` above) must be a real nonzero figure,
    // never `total  0 B` — the smoking gun of reading sizes from the stale system
    // syncdb (cached installed archive → `0`) instead of the freshly-synced db.
    let screen = pty.screen();
    assert!(
        !screen.contains("total  0 B"),
        "change-set total is `0 B` — preview sizes look stale (read from the \
         system syncdb whose installed-version archive is cached) rather than \
         the freshly synced db's new version\n--- screen ---\n{screen}\n--- end ---"
    );
    pty.send(b"\r");

    // The sudo gate for the partial `pacman -Syu`.
    pty.expect("sudo gate", |s| s.contains("Continue?"));
    pty.send(b"\r");

    // Wait for the upgrade to finish (a full `pacman -Syu` syncs + downloads, so
    // it runs well past the gate) before driving `show`: sending it mid-upgrade
    // races the install and the buffered input is dropped when rustyline re-enters
    // raw mode. A clean apply prints `done` and clears the cart.
    pty.expect("apply finished", |s| s.contains("done"));
    // `show` then reports the cart empty — the shell-side proof the upgrade landed.
    pty.send(b"show\r");
    pty.expect("cart cleared after apply", |s| s.contains("cart is empty"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_UPGRADE_E2E_OK");
}
