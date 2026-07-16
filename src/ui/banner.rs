//! The shell's launch splash: a horned ox beside the AUROX lettering,
//! set in figlet's slant font — the italic of ASCII art. Uppercase on
//! purpose: slant's lowercase `a` and `o` bowls are near-identical, and a
//! banner that reads "ourox" defeats itself.

use super::grid::{Paint, Table};
use console::style;

/// The splash art, one row per line: the fixed-width ox-head column and the
/// lettering it flanks, kept as separate columns so each side takes its own
/// color without re-splitting rendered strings. The head fills the bottom
/// three rows, standing on the lettering's baseline.
const ART: &[(&str, &str)] = &[
    ("        ", "    ___   __  ______  ____ _  __"),
    ("        ", r"   /   | / / / / __ \/ __ \ |/ /"),
    ("  ^__^  ", "  / /| |/ / / / /_/ / / / /   /"),
    ("  (oo)  ", " / ___ / /_/ / _, _/ /_/ /   |"),
    ("  (__)  ", r"/_/  |_\____/_/ |_|\____/_/|_|"),
];

/// The crate version as the splash tags it (`v0.2.0`).
const VERSION_TAG: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// Render the launch splash — the ox in yellow, the lettering in pacman's
/// bold headline blue, the version tag dim at the end of the last row.
///
/// The shell shows this once per session behind the `banner` config knob,
/// after the first-launch question (art must never bury a prompt). `Paint`
/// decides color exactly like every other renderer, so `--color never` —
/// and tests, which pin [`Paint::Plain`] — get the plain bytes.
pub fn launch_banner(paint: Paint) -> Table {
    let mut out = Table::new();
    for (i, (ox, letters)) in ART.iter().enumerate() {
        let mut line = if paint.colored() {
            format!("{}{}", style(*ox).yellow(), style(*letters).bold().blue())
        } else {
            format!("{ox}{letters}")
        };
        if i == ART.len() - 1 {
            line.push_str("  ");
            if paint.colored() {
                line.push_str(&super::dim(VERSION_TAG).to_string());
            } else {
                line.push_str(VERSION_TAG);
            }
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::assert_contains;

    /// The plain splash: one line per art row with no ANSI escapes, the ox
    /// beside the lettering, and the crate version tagged onto the last row.
    #[test]
    fn plain_splash_shape() {
        let table = launch_banner(Paint::Plain);
        let lines = table.lines();
        assert_eq!(lines.len(), ART.len(), "one line per art row: {lines:?}");
        for line in lines {
            assert!(
                !line.contains('\u{1b}'),
                "plain must carry no ANSI: {line:?}"
            );
        }
        assert_contains!(lines[3], "(oo)");
        assert_contains!(lines[4], VERSION_TAG);
    }

    /// The colored splash carries ANSI styling on every row and strips back
    /// to the exact plain bytes, so the two paints can't drift apart.
    #[test]
    fn colored_splash_strips_to_plain() {
        // `console` gates styling on its own stdout-TTY detection at render
        // time; force it on so the colored branch is observable when the test
        // runs piped (plain `cargo test`), not only under makepkg's tty.
        console::set_colors_enabled(true);
        let plain = launch_banner(Paint::Plain);
        let colored = launch_banner(Paint::Colored);
        for (c, p) in colored.lines().iter().zip(plain.lines()) {
            assert_contains!(c, "\u{1b}[");
            assert_eq!(
                console::strip_ansi_codes(c),
                *p,
                "colored row must strip to the plain bytes"
            );
        }
    }
}
