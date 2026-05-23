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
    bar_bytes, bar_bytes_streaming, bar_count, bar_sideband, promote_byte_bar, promote_count_bar,
    spinner, tick, TICK_PERIOD,
};
pub use prompts::{confirm, select_pkgnames};
pub use tables::{install_table, pkg_list, select_upgrades, upgrade_table, UpgradeSelection};

use console::{style, Term};
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
pub fn set_color(mode: ColorMode) {
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

/// Render `text` as supporting/secondary UI text — mid-gray (color 244) italic.
///
/// Reads clearly without competing with the bright primary text. Use for
/// hint annotations, last-built timestamps, anything the eye should *not*
/// lock onto.
pub fn dim(text: impl AsRef<str>) -> console::StyledObject<String> {
    style(text.as_ref().to_owned()).color256(244).italic()
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
}
