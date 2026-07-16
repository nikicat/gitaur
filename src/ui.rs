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

mod banner;
mod cells;
mod change_set;
mod cost;
mod gix_progress;
mod grid;
mod progress;
mod prompts;
mod search_table;
mod tables;

pub use banner::launch_banner;
pub use change_set::{ApprovalCell, ChangeSet, TxnRoot};
pub use cost::PreviewMetrics;
pub use gix_progress::{GixProgress, Operation};
pub use grid::{Cell, Col, Grid, GridRow, Paint, Table, Width};
pub use progress::{
    TICK_PERIOD, bar_bytes, bar_bytes_streaming, bar_count, bar_sideband, promote_byte_bar,
    promote_count_bar, spinner, tick,
};
pub use prompts::{AurSetupChoice, aur_setup_prompt, confirm, confirm_default_no, select_pkgnames};
pub use search_table::{
    InstallState, MatchNote, RowNumbers, SearchRow, search_result, search_table,
};
pub use tables::{UpgradeSelection, install_table, upgrade_table};

use crate::units::ByteSize;
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
/// color on/off even when output isn't a TTY (e.g. `aurox --color always |
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

/// Format a byte count in IEC binary units (`512 B`, `1.00 KiB`, `3.42 GiB`).
///
/// Thin wrapper over [`ByteSize`]'s `Display` (where the formatting rules
/// are documented), for call sites that carry a raw `u64` from an untyped
/// boundary. Code holding a [`ByteSize`] should just display it.
pub fn human_bytes(bytes: u64) -> String {
    ByteSize::new(bytes).to_string()
}

/// Format a build [`Duration`](std::time::Duration) as the change-set preview
/// shows it: `42s`, `3m 17s`, `2h 8m`.
///
/// Takes a `Duration` (the time domain type, matching [`human_age`]) rather than
/// a bare seconds count, so callers don't downgrade to `u64` at the boundary.
/// Two units max (the leading non-zero plus the next down) — enough resolution
/// for "is this a quick rebuild or an evening's worth of compile time?" without
/// precision noise. Sub-second figures are not meaningful for builds (makepkg's
/// own setup is ~1 s) so floors at `0s`.
pub fn human_duration(d: std::time::Duration) -> String {
    let seconds = d.as_secs();
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// Format an elapsed age as a single coarse unit — `3d`, `5h`, `12m`, or `now`.
///
/// The AUR "last modified" column (yay-style) only needs "how stale is this
/// PKGBUILD?", so one unit is enough; rounds down to the largest non-zero unit
/// and floors sub-minute ages at `now`.
pub fn human_age(age: std::time::Duration) -> String {
    let secs = age.as_secs();
    if secs >= 86_400 {
        format!("{}d", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{}h", secs / 3_600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        "now".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Single-unit "time since" rounding for the AUR last-modified column:
    /// floors at `now` under a minute, then the largest non-zero of m/h/d.
    #[test]
    fn human_age_single_coarse_unit() {
        use std::time::Duration;
        let age = |s| human_age(Duration::from_secs(s));
        assert_eq!(age(0), "now");
        assert_eq!(age(59), "now");
        assert_eq!(age(60), "1m");
        assert_eq!(age(59 * 60), "59m");
        assert_eq!(age(3_600), "1h");
        assert_eq!(age(23 * 3_600), "23h");
        assert_eq!(age(86_400), "1d");
        assert_eq!(age(3 * 86_400 + 5 * 3_600), "3d");
    }

    #[test]
    fn human_bytes_delegates_to_byte_size() {
        // The unit/precision matrix is pinned in `units::tests`; this only
        // guards the wrapper's plumbing.
        assert_eq!(human_bytes(1536), "1.50 KiB");
    }

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
