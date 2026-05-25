//! Colored user-facing CLI output (banners, package lists, progress bars, prompts).
//!
//! Built on `console` (styling), `indicatif` (bars/spinners), and `dialoguer`
//! (prompts) — the pacman/yay-style UI stack. Independent of [`tracing`],
//! which carries diagnostic events for developers and stays silent unless
//! `RUST_LOG` enables it.
//!
//! Progress-bar conventions in this module:
//! - `{prefix}` carries the **fixed** row label (`objects`, `received`, …).
//! - `{msg}` / `{wide_msg}` carry **streaming** content (e.g. sideband lines).
//!
//! Splitting the two lets callers `set_message` without clobbering the label.

mod gix_progress;
mod progress;
mod prompts;
mod tables;

pub use gix_progress::GixProgress;
pub use progress::{
    TICK_PERIOD, bar_bytes, bar_bytes_streaming, bar_count, bar_sideband, promote_byte_bar,
    promote_count_bar, spinner, tick,
};
pub use prompts::{confirm, select_pkgnames};
pub use tables::{UpgradeSelection, install_table, pkg_list, select_upgrades, upgrade_table};

use console::{Term, style};
use std::sync::OnceLock;

/// User preference for terminal color output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColorMode {
    /// Detect TTY/`NO_COLOR`/etc. at print time.
    #[default]
    Auto,
    /// Force ANSI escapes on, even when stderr isn't a TTY.
    Always,
    /// Suppress all color escapes.
    Never,
}

static COLOR: OnceLock<ColorMode> = OnceLock::new();

/// Install the process-wide color mode. First caller wins.
///
/// Also drives `console`'s own global gate so `always`/`never` actually force
/// color on/off even when output isn't a TTY (e.g. `gitaur --color always |
/// cat`). Without this, `color_on` would pick the colored code branch but
/// every `console::style(...)` would still strip its escapes on a pipe.
/// `auto` leaves console's built-in per-stream TTY detection untouched.
pub fn set_color(mode: ColorMode) {
    match mode {
        ColorMode::Always => {
            console::set_colors_enabled(true);
            console::set_colors_enabled_stderr(true);
        }
        ColorMode::Never => {
            console::set_colors_enabled(false);
            console::set_colors_enabled_stderr(false);
        }
        ColorMode::Auto => {}
    }
    COLOR.set(mode).ok();
}

pub fn color_on() -> bool {
    match COLOR.get().copied().unwrap_or(ColorMode::Auto) {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => Term::stderr().features().colors_supported(),
    }
}

/// Print a top-level status line (`:: msg`) in bold blue.
pub fn info(msg: &str) {
    if color_on() {
        eprintln!("{} {}", style("::").bold().blue(), style(msg).bold());
    } else {
        eprintln!(":: {msg}");
    }
}

/// Print a build-phase banner (`==> msg`) in bold green.
pub fn step(msg: &str) {
    if color_on() {
        eprintln!("{} {}", style("==>").bold().green(), style(msg).bold());
    } else {
        eprintln!("==> {msg}");
    }
}

/// Print a warning line in yellow.
pub fn warn(msg: &str) {
    if color_on() {
        eprintln!("{} {}", style("warning:").yellow().bold(), msg);
    } else {
        eprintln!("warning: {msg}");
    }
}

/// Print an error line in red.
pub fn error(msg: &str) {
    if color_on() {
        eprintln!("{} {}", style("error:").red().bold(), msg);
    } else {
        eprintln!("error: {msg}");
    }
}

/// Print a detail/follow-up line in cyan.
pub fn note(msg: &str) {
    if color_on() {
        eprintln!("{} {}", style("->").cyan(), msg);
    } else {
        eprintln!("-> {msg}");
    }
}

/// Emit terminal BEL (0x07) to stderr to nudge a walked-away user.
///
/// Call sites: long-running operations where the prompt may fire long
/// after the last interaction (e.g. mid-build sudo escalation, 5–30 min
/// into an AUR build). No-op when stderr isn't a TTY so logfiles and CI
/// pipes stay clean. Writes via the raw byte API (not `eprint!`) to
/// bypass any future `console`/styling layers that might filter control
/// chars.
pub fn bell() {
    use std::io::{IsTerminal, Write};
    let mut err = std::io::stderr().lock();
    if err.is_terminal() {
        err.write_all(b"\x07").ok();
        err.flush().ok();
    }
}

/// Render `text` as supporting/secondary UI text — mid-gray (color 244) italic.
///
/// Reads clearly without competing with the bright primary text. Use for
/// hint annotations, last-built timestamps, anything the eye should *not*
/// lock onto.
pub fn dim(text: impl AsRef<str>) -> console::StyledObject<String> {
    style(text.as_ref().to_owned()).color256(244).italic()
}

/// Style a repository name (`core`, `extra`, `aur`, …) as a colored list-row
/// prefix, the way yay does.
///
/// Each name hashes deterministically to one of six bold ANSI colors, so
/// `core`, `extra`, and `aur` come out visually distinct but stable per repo.
///
/// Replicates yay's `text.ColorHash` byte-for-byte — djb2 seeded at 5381 in
/// 64-bit wrapping arithmetic, then `hash % 6` over the bold colors red,
/// green, yellow, blue, magenta, cyan (ANSI 31–36).
///
/// Always emits color codes; callers gate on [`color_on`] and keep a plain
/// branch (same convention as [`dim`]). The slash and package name stay the
/// caller's responsibility so width math runs on the unstyled text.
pub fn repo(name: impl AsRef<str>) -> console::StyledObject<String> {
    let name = name.as_ref();
    let mut hash: u64 = 5381;
    for b in name.bytes() {
        hash = u64::from(b).wrapping_add((hash << 5).wrapping_add(hash));
    }
    let styled = style(name.to_owned()).bold();
    match hash % 6 {
        0 => styled.red(),
        1 => styled.green(),
        2 => styled.yellow(),
        3 => styled.blue(),
        4 => styled.magenta(),
        _ => styled.cyan(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The upgrade-table header is auxiliary information — it must render in
    /// the same gray-italic style as phase hints, never bold. Pins the ANSI
    /// codes so a refactor that re-bolds the header fails loudly.
    #[test]
    fn dim_is_italic_color244_not_bold() {
        let out = dim("Repo upgrades (3)").force_styling(true).to_string();
        assert!(
            out.contains("\u{1b}[38;5;244m"),
            "missing color 244: {out:?}"
        );
        assert!(out.contains("\u{1b}[3m"), "missing italic: {out:?}");
        assert!(
            !out.contains("\u{1b}[1m"),
            "header should not be bold: {out:?}"
        );
    }

    /// `repo` must reproduce yay's `text.ColorHash` so prefixes look identical
    /// across the two tools. These pins are the colors yay assigns today —
    /// core→yellow, extra→green, multilib→cyan, aur→blue — computed from the
    /// same djb2/`%6` mapping. A change to the hash or color table breaks here.
    #[test]
    fn repo_colors_match_yay_colorhash() {
        let out = |name: &str| repo(name).force_styling(true).to_string();
        let bold_colored = |name: &str, color: &str| {
            let s = out(name);
            s.contains(color) && s.contains("\u{1b}[1m")
        };
        assert!(bold_colored("core", "\u{1b}[33m"), "core not bold yellow");
        assert!(bold_colored("extra", "\u{1b}[32m"), "extra not bold green");
        assert!(
            bold_colored("multilib", "\u{1b}[36m"),
            "multilib not bold cyan"
        );
        assert!(bold_colored("aur", "\u{1b}[34m"), "aur not bold blue");
    }
}
