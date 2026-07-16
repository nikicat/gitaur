//! End-to-end driver for the shell's `upgrade` procedure (REPL phase 4), used
//! by `tests/container/extended/04_shell_upgrade_repo.sh`.
//!
//! No-arg `aurox` opens the shell. This drives the upgrade flow against an
//! installed-but-outdated **repo** package: `upgrade` refreshes + seeds the
//! pending upgrade (auto-approved, since it's a repo row), then `apply` prints
//! the one-line cost summary, takes the sudo gate, and runs the partial
//! `pacman -Syu`. A clean apply empties the cart, which `show` confirms.
//!
//! It also folds in the synced-db **size guard** (the retired
//! `05_loop_size_from_synced_db` test): the size figure is parsed off the
//! `-> total  📥 …` line and must be a real nonzero value. `0 B` is the
//! smoking gun of reading sizes from the stale system syncdb (whose
//! installed-version archive is cached → `0`) instead of the freshly-synced
//! db carrying the new version.

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

    // The size guard from the retired `05_loop_size_from_synced_db`: pump until
    // the `-> total  📥 …` line renders with a parseable figure — the row
    // expect above can match while the table is still streaming, so a one-shot
    // screen grab races the total line — then require a real nonzero value.
    // `0 B` is the smoking gun of reading sizes from the stale system syncdb
    // (cached installed archive → `0`) instead of the freshly-synced db; an
    // unparseable figure (`?`) times the expect out and dumps the screen
    // instead of slipping past a substring needle.
    let size_re =
        regex::Regex::new(r"(?m)^-> total\s+📥 >?([0-9]+(?:\.[0-9]+)?) (?:B|[KMGT]iB) *$").unwrap();
    pty.expect("parseable change-set total", |s| size_re.is_match(s));
    let screen = pty.screen();
    let size: f64 = size_re
        .captures(&screen)
        .expect("expect() pumped until the total line matched")[1]
        .parse()
        .expect("regex-matched figure is a number");
    assert!(
        size > 0.0,
        "change-set total is `0` — preview sizes look stale (read from the \
         system syncdb whose installed-version archive is cached) rather than \
         the freshly synced db's new version\n--- screen ---\n{screen}\n--- end ---"
    );

    // The explicit `apply` is the consent — no transaction confirm (phase 5a
    // folded the old apply-time change-set table into `show`/`upgrade`, so the
    // table — and its `this batch` total — prints at `upgrade` above). The
    // first prompt after the one-line cost summary is the sudo gate for the
    // partial `pacman -Syu`.
    pty.send(b"apply\r");
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
