//! Headless harness for the regression test in `tests/picker_artifacts.rs`.
//!
//! Prints a handful of sentinel lines on stderr, then opens the interactive
//! upgrade picker with a synthetic plan large enough that the cursor has to
//! scroll. The test runner drives this binary inside a PTY (optionally
//! wrapped in `podman`), sends arrow-down keys, parses the resulting VT100
//! stream into a screen grid, and asserts the sentinels are still on screen
//! — i.e. dialoguer's redraw didn't eat the lines above the prompt.

use gitaur::config::Config;
use gitaur::pacman::invoke::{PkgUpgrade, REPO_AUR};
use gitaur::ui::{self, ColorMode};
use std::io::Write;

fn main() {
    // Force ANSI escapes regardless of TTY detection: the bug we're guarding
    // against only triggers when items would otherwise carry color, so the
    // test must pin the colored branch on every run. Two switches are
    // needed — `ui::set_color` toggles our `color_on()` gate, but the
    // `console` crate uses its own atomic which only auto-enables for real
    // TTYs (and false-negatives under some PTY harnesses).
    ui::set_color(ColorMode::Always);
    console::set_colors_enabled(true);
    console::set_colors_enabled_stderr(true);

    let sentinels: usize = std::env::var("PICKER_E2E_SENTINELS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let mut stderr = std::io::stderr().lock();
    for i in 1..=sentinels {
        writeln!(stderr, "SENTINEL-{i:02}").unwrap();
    }
    stderr.flush().unwrap();

    let plan = synthetic_plan(20);
    let cfg = Config::default();
    let sel = ui::select_upgrades(&plan, &cfg, false).expect("picker failed");

    // Echo the selection on stdout so the test can confirm the prompt ran
    // to completion (Enter was actually consumed) rather than aborting on a
    // dropped stdin.
    println!("picked-repo={}", sel.repo.join(","));
    println!("picked-aur={}", sel.aur.join(","));
}

fn synthetic_plan(n: usize) -> Vec<PkgUpgrade> {
    (0..n)
        .map(|i| PkgUpgrade {
            repo: if i % 3 == 0 {
                REPO_AUR.into()
            } else {
                "extra".into()
            },
            name: format!("pkg-{i:02}"),
            old_ver: "1.0.0-1".into(),
            new_ver: "1.0.1-1".into(),
        })
        .collect()
}
