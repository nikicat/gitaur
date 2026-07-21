//! End-to-end driver for the explicit-AUR-pin path, used by
//! `tests/container/extended/40_shell_aur_pin.sh`.
//!
//! `aurpin` exists BOTH as a sync package (`aurpin-repo` fixture, 1.0) and as
//! an AUR pkgbase (`aurpin` fixture, 9.0) — a genuine name collision. Pacman
//! precedence would route a bare `aurpin` to the sync package. This drives the
//! shell to pick the AUR *row* by number, which pins the choice to the AUR, and
//! asserts the pin wins:
//!
//! ```text
//!   (launch `aurox ^aurpin$`) → banner + seeded list with both rows
//!   add <aur row #>           → staged as `aurpin (aur)`  ← pin survives
//!   approve aurpin; apply      → builds + installs the AUR 9.0 (not sync 1.0)
//! ```
//!
//! The `.sh` runs `aurox -Sy` first (so the index carries the AUR `aurpin`) and,
//! after this driver exits clean, asserts `aurpin` is installed at the AUR
//! version — the tell that the pin overrode pacman precedence.

use pty_harness::{Pty, has};

/// The 1-based row number of the AUR `aurpin` row. Search renders each hit as
/// `<n>  <repo>/<name> <version> [badge]`, so the AUR row carries the token
/// `aur/aurpin` and the sync row `local-repo/aurpin`. Robust to the ranking
/// order of the two collision rows, which the pin test must not depend on.
fn aur_row_number(screen: &str) -> Option<u32> {
    for line in screen.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.contains(&"aur/aurpin")
            && let Ok(n) = f.first()?.parse::<u32>()
        {
            return Some(n);
        }
    }
    None
}

/// True once BOTH collision rows are on screen — the AUR `aur/aurpin` row and a
/// non-AUR (sync) `…/aurpin` row — so the pick is a real disambiguation.
fn both_rows(screen: &str) -> bool {
    let toks: Vec<&str> = screen.split_whitespace().collect();
    let has_aur = toks.contains(&"aur/aurpin");
    let has_sync = toks
        .iter()
        .any(|&w| w != "aur/aurpin" && w.ends_with("/aurpin"));
    has_aur && has_sync
}

fn main() {
    // Launch straight into the seeded, exact-name search so the list is exactly
    // the two `aurpin` rows.
    let mut pty = Pty::spawn_aurox_args(&["^aurpin$"]);
    pty.expect("shell banner", |s| s.contains("aurox shell"));
    pty.expect("both collision rows", both_rows);

    // Pick the AUR row by its number — that is what pins the choice to the AUR.
    let n = aur_row_number(&pty.screen()).expect("aur row must be on screen");
    pty.send(format!("add {n}\r").as_bytes());
    // The staged label proves the pin took: the row's `aur` source, not the
    // repo namesake that classifying the bare name would have chosen.
    pty.expect("staged from the AUR row", |s| has(s, "staged aurpin (aur)"));

    // Clear the review gate, then apply. The explicit `apply` is the consent;
    // no deps are pulled in, so the only prompt is the sudo gate.
    pty.send(b"approve aurpin\r");
    pty.expect("approved", |s| has(s, "approved aurpin"));

    pty.send(b"apply\r");
    pty.expect("sudo gate", |s| s.contains("Continue?"));
    pty.send(b"\r");
    pty.expect("apply finished", |s| s.contains("done"));

    pty.send(b"quit\r");
    pty.finish_clean();
    println!("SHELL_AUR_PIN_E2E_OK");
}
