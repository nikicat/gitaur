//! The search list's **row layout** — one entry point ([`SearchList`]) over two
//! renderings of the same ranked rows, chosen by the [`SearchLayout`] config knob.
//!
//! Two shapes, the same information:
//!
//! - **single-line** ([`super::search_table`]) — an aligned grid, dense: one
//!   scannable row with `repo · name · version · installed · freshness` columns
//!   and the description in the tail. Best for *resolving a known name* and for
//!   pipes (a stable one-record-per-line form).
//! - **two-line** ([`double_line`]) — pacman's shape: a
//!   `№ repo/name version [installed] [age]` headline, then the description on
//!   its own indented line. Best for *comparing candidates* and on a narrow
//!   terminal, where the single-line description would wrap mid-word after the
//!   columns.
//!
//! [`SearchLayout::Auto`] (the default) picks per render: the dense grid when a
//! row fits the terminal, the roomy two-line form when it would overflow — the
//! same width-adaptive spirit as `color = auto`. yay/paru ship this exact toggle
//! (`--singlelineresults` / `--doublelineresults`); aurox makes it a config knob.
//!
//! Ordering is one concern the dispatcher owns so no call site re-derives it:
//! rows arrive **best-first** (row 1 is the best match, and its `№` is assigned
//! from that order), and [`SearchList::render`] emits them **best-last**
//! (bottom-up) at *row* granularity — so a two-line row's headline and
//! description stay together, the bug a flat line-reverse ([`Table::reversed`])
//! would cause. The strongest match lands nearest the prompt with the low,
//! easy-to-type number.

use super::grid::{Paint, Table, Width};
use super::search_table::{installed_marker_text, search_table};
use super::{RowNumbers, SearchRow, dim, repo as repo_style};
use console::style;
use serde::{Deserialize, Serialize};

/// How the interactive/pipe search list lays out each row — the typed value
/// behind the `search_layout` config knob (a named enum, not a string, mirroring
/// [`ColorMode`](super::ColorMode)).
///
/// [`Auto`](Self::Auto) is width-adaptive; [`Single`](Self::Single) /
/// [`Double`](Self::Double) force a shape. Only the *interactive* list and the
/// bare-term pipe listing consult it — `-Ss` stays two-line for pacman byte
/// parity regardless (see [`super::search_result`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchLayout {
    /// Single-line grid when a row fits the terminal, two-line when it would
    /// overflow; a non-terminal (pipe/file) always renders single-line.
    #[default]
    Auto,
    /// Always the aligned single-line grid.
    Single,
    /// Always the pacman-style two-line rows.
    Double,
}

/// A resolved (no-`Auto`) row shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Single,
    Double,
}

/// The ranked search rows plus how to lay them out — the one entry point every
/// search surface renders through.
///
/// A named request rather than a parameter caravan (mirrors
/// [`ChangeSet`](super::ChangeSet)): the rows are **best-first**, `numbers` says
/// whether the shell's selector `№` column leads each row, and `layout` is the
/// user's knob. [`Self::render`] turns it into the printable, best-last
/// [`Table`].
pub struct SearchList<'a> {
    /// The ranked hits, best match first (its `№` is 1).
    pub rows: &'a [SearchRow],
    /// Whether the selector `№` column leads each row (shell) or not (pipe).
    pub numbers: RowNumbers,
    /// The configured row layout.
    pub layout: SearchLayout,
}

impl SearchList<'_> {
    /// Render best-last (bottom-up) in the resolved layout.
    ///
    /// `term_width` is the interactive terminal [`Width`], [`None`] for a
    /// pipe/file (no meaningful width) — injected, not read here, so the layout
    /// choice stays deterministic and testable ([`super::term_width`] supplies
    /// it at the call sites). Numbers are assigned best-first regardless of the
    /// emitted order, so `add 1` always addresses the top match.
    pub fn render(&self, paint: Paint, term_width: Option<Width>) -> Table {
        let groups = match self.shape(term_width) {
            Shape::Single => single_line(self.rows, self.numbers, paint),
            Shape::Double => double_line(self.rows, self.numbers, paint),
        };
        // Best-last at row granularity: reverse the group order (a two-line
        // row's headline + description ride in one group, so they stay paired),
        // never the flat line list.
        let mut table = Table::new();
        for group in groups.iter().rev() {
            for line in group {
                table.push(line.clone());
            }
        }
        table
    }

    /// Resolve [`SearchLayout::Auto`] against the terminal width: two-line when
    /// the widest single-line row would overflow `term_width`, single-line
    /// otherwise (and always single-line with no terminal to measure against).
    fn shape(&self, term_width: Option<Width>) -> Shape {
        match self.layout {
            SearchLayout::Single => Shape::Single,
            SearchLayout::Double => Shape::Double,
            // No terminal to measure against (a pipe) stays dense single-line.
            SearchLayout::Auto => {
                let overflows =
                    term_width.is_some_and(|w| single_line_overflows(self.rows, self.numbers, w));
                if overflows {
                    Shape::Double
                } else {
                    Shape::Single
                }
            }
        }
    }
}

/// Whether the aligned single-line layout's widest row exceeds `term_width`.
/// Measured on the *plain* render — visible width == char count there, no ANSI
/// to strip — via [`Width::of`], so the decision is independent of paint.
fn single_line_overflows(rows: &[SearchRow], numbers: RowNumbers, term_width: Width) -> bool {
    search_table(rows, numbers, Paint::Plain)
        .lines()
        .iter()
        .any(|l| Width::of(l) > term_width)
}

/// The single-line layout as one line-group per row (each group is exactly one
/// line), so [`SearchList::render`] reverses rows uniformly across both shapes.
fn single_line(rows: &[SearchRow], numbers: RowNumbers, paint: Paint) -> Vec<Vec<String>> {
    search_table(rows, numbers, paint)
        .lines()
        .iter()
        .map(|l| vec![l.clone()])
        .collect()
}

/// The two-line, pacman-shaped layout: per row a `№ repo/name version
/// [installed] [age]` headline group, followed by the indented description line
/// when the row has a description or a match-note.
fn double_line(rows: &[SearchRow], numbers: RowNumbers, paint: Paint) -> Vec<Vec<String>> {
    rows.iter()
        .enumerate()
        .map(|(i, row)| {
            let mut group = vec![headline(i, row, numbers, paint)];
            if let Some(desc) = desc_line(row, paint) {
                group.push(desc);
            }
            group
        })
        .collect()
}

/// The two-line headline: `{№} repo/name version [installed] [age]`, single
/// spaces between the present segments (pacman's shape). The `№` is the shell's
/// selector index, floored at three digits to match the single-line grid.
///
/// The **identity** segments read bold on every row — a bold headline
/// independent of install status (hashed-bold repo, [bold name](repo_name), bold
/// version, and a bold `[installed]` marker). The installed marker keeps its
/// currency color (**dim** current, **yellow** behind — same signal as the
/// single-line installed column, not the `-Ss` bold cyan). The freshness tag
/// alone keeps its band styling ([`Freshness::tag`](super::Freshness::tag)): its
/// weight *is* the risk signal (loud caution → faded stale), so it is not
/// flattened to a uniform bold.
fn headline(i: usize, row: &SearchRow, numbers: RowNumbers, paint: Paint) -> String {
    let prefix = match numbers {
        RowNumbers::Numbered => format!("{:>3}  ", i + 1),
        RowNumbers::Plain => String::new(),
    };
    let mut segs = vec![repo_name(row, paint)];
    if let Some(v) = row.new_ver.as_ref() {
        let v = v.as_str();
        segs.push(if paint.colored() {
            style(v.to_owned()).bold().to_string()
        } else {
            v.to_owned()
        });
    }
    if let Some(m) = installed_tag(row, paint) {
        segs.push(m);
    }
    if let Some(f) = &row.freshness {
        segs.push(f.tag(paint));
    }
    format!("{prefix}{}", segs.join(" "))
}

/// `repo/name` for the headline — the repo in its hashed (bold) color, the name
/// **bold on every row** so the first line reads as one bold headline regardless
/// of install status (pacman's `-Ss` bolds the name unconditionally too;
/// installed-ness is signalled by the `[installed]` marker, not the name weight).
/// The `/` and name join the repo as one segment so no stray column math is
/// needed.
fn repo_name(row: &SearchRow, paint: Paint) -> String {
    let (repo, name) = (row.repo.as_str(), row.name.as_str());
    if !paint.colored() {
        return format!("{repo}/{name}");
    }
    format!("{}/{}", repo_style(repo), style(name.to_owned()).bold())
}

/// The installed marker for the headline — the shared [`installed_marker_text`]
/// (`[installed]` / `[installed: X]`), **bold** (it rides the bold headline) and
/// currency-colored: **yellow** when an upgrade waits
/// ([`SearchRow::upgrade_from`]), **dim** when current (or a newer local build).
/// [`None`] for a not-installed row.
fn installed_tag(row: &SearchRow, paint: Paint) -> Option<String> {
    let text = installed_marker_text(row)?;
    Some(if !paint.colored() {
        text
    } else if row.upgrade_from().is_some() {
        style(text).yellow().bold().to_string()
    } else {
        dim(text).bold().to_string()
    })
}

/// The indented description line: the (undimmed, pacman-style) description plus
/// the dimmed match-note when present. [`None`] when the row has neither, so no
/// blank second line is emitted.
fn desc_line(row: &SearchRow, paint: Paint) -> Option<String> {
    let desc = row.desc.as_deref().filter(|d| !d.is_empty());
    let note = row.note.as_ref().map(ToString::to_string);
    if desc.is_none() && note.is_none() {
        return None;
    }
    let mut body = desc.unwrap_or("").to_owned();
    if let Some(note) = note {
        if !body.is_empty() {
            body.push_str("  ");
        }
        body.push_str(&if paint.colored() {
            dim(&note).to_string()
        } else {
            note
        });
    }
    Some(format!("{DESC_INDENT}{body}"))
}

/// Description-line indent — sits the description clear of the headline's `№`
/// prefix so the two-line row reads as one nested unit.
const DESC_INDENT: &str = "      ";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::{PkgName, RepoName};
    use crate::ui::{AgeScale, AgeThresholds, Freshness, InstallState};
    use crate::units::UnixTime;
    use crate::version::Version;
    use crate::{assert_contains, assert_not_contains};

    /// A search row from domain-typed parts, description derived from the name.
    fn row(repo: &str, name: &str, install: InstallState, new: Option<&str>) -> SearchRow {
        SearchRow {
            repo: RepoName::from(repo),
            name: PkgName::from(name),
            install,
            new_ver: new.map(Version::from),
            desc: Some(format!("{name} description")),
            note: None,
            freshness: None,
        }
    }

    /// A freshness badge `days_old` days old, against a fixed clock.
    fn badge(days_old: i64) -> Option<Freshness> {
        let sec_per_day = 86_400;
        let now = UnixTime::new(1_000 * sec_per_day).system_time()?;
        let scale = AgeScale::at(now, AgeThresholds::from_days(2, 180, 730));
        scale.badge(UnixTime::new((1_000 - days_old) * sec_per_day), false)
    }

    /// `SearchLayout` parses from the lowercase toml spellings and defaults to
    /// `Auto` — the config knob's on-disk contract. (toml deserializes from a
    /// keyed table, so the spellings ride a wrapper key.)
    #[test]
    fn layout_parses_lowercase_and_defaults_auto() {
        #[derive(Deserialize, Serialize)]
        struct W {
            v: SearchLayout,
        }
        assert_eq!(SearchLayout::default(), SearchLayout::Auto);
        for (s, want) in [
            ("auto", SearchLayout::Auto),
            ("single", SearchLayout::Single),
            ("double", SearchLayout::Double),
        ] {
            let w: W = toml::from_str(&format!("v = \"{s}\"")).unwrap();
            assert_eq!(w.v, want, "{s}");
        }
        let out = toml::to_string(&W {
            v: SearchLayout::Double,
        })
        .unwrap();
        assert!(out.contains("v = \"double\""), "{out:?}");
    }

    /// `Double` renders each row as a headline + an indented description line;
    /// `add 1` still addresses the best match because numbers are assigned
    /// best-first even though the list prints best-last.
    #[test]
    fn double_renders_headline_then_indented_desc_best_last() {
        let rows = vec![
            row("aur", "best-match", InstallState::NotInstalled, Some("1-1")),
            row(
                "extra",
                "worse-match",
                InstallState::NotInstalled,
                Some("2-1"),
            ),
        ];
        let table = SearchList {
            rows: &rows,
            numbers: RowNumbers::Numbered,
            layout: SearchLayout::Double,
        }
        .render(Paint::Plain, Some(Width::cols(100)));
        let lines = table.lines();
        // Two rows × (headline + desc) = 4 lines, worst-first (best-last).
        assert_eq!(lines.len(), 4);
        assert!(
            lines[0].starts_with("  2  extra/worse-match 2-1"),
            "{lines:?}"
        );
        assert_eq!(lines[1], "      worse-match description");
        // The best match prints last (nearest the prompt) as row 1.
        assert!(lines[2].starts_with("  1  aur/best-match 1-1"), "{lines:?}");
        assert_eq!(lines[3], "      best-match description");
    }

    /// The two-line headline carries the installed marker and the freshness tag;
    /// a not-installed row shows neither installed marker.
    #[test]
    fn double_headline_marks_installed_and_freshness() {
        let mut installed = row(
            "aur",
            "have-it",
            InstallState::Installed(Version::from("1.0-1")),
            Some("1.1-1"),
        );
        installed.freshness = badge(3);
        let plain = SearchList {
            rows: std::slice::from_ref(&installed),
            numbers: RowNumbers::Plain,
            layout: SearchLayout::Double,
        }
        .render(Paint::Plain, Some(Width::cols(100)));
        let headline = &plain.lines()[0];
        // Behind: pacman marker names the local version, freshness tag present.
        assert_contains!(headline, "[installed: 1.0-1]");
        assert_contains!(headline, "[3d]");

        let not = row("aur", "fresh-only", InstallState::NotInstalled, Some("1-1"));
        let table = SearchList {
            rows: std::slice::from_ref(&not),
            numbers: RowNumbers::Plain,
            layout: SearchLayout::Double,
        }
        .render(Paint::Plain, Some(Width::cols(100)));
        assert_not_contains!(table.lines()[0], "[installed");
    }

    /// The installed marker is currency-colored on the two-line headline —
    /// yellow when an upgrade waits, dim when current — matching the single-line
    /// installed column (not the `-Ss` bold cyan).
    #[test]
    fn double_installed_marker_is_currency_colored() {
        console::set_colors_enabled(true);
        let render = |installed: &str, available: &str| {
            let r = row(
                "extra",
                "clang",
                InstallState::Installed(Version::from(installed)),
                Some(available),
            );
            SearchList {
                rows: std::slice::from_ref(&r),
                numbers: RowNumbers::Plain,
                layout: SearchLayout::Double,
            }
            .render(Paint::Colored, Some(Width::cols(100)))
            .lines()[0]
                .clone()
        };
        let behind = render("18.0.0-1", "18.1.0-1");
        let current = render("18.1.0-1", "18.1.0-1");
        // Bold (it rides the bold headline) plus the currency color.
        let (yellow_behind, dim_current) = (
            style("[installed: 18.0.0-1]").yellow().bold().to_string(),
            dim("[installed]").bold().to_string(),
        );
        assert_contains!(behind, yellow_behind.as_str());
        assert_contains!(current, dim_current.as_str());
    }

    /// The two-line headline's identity segments (name, version) are **bold on
    /// every row**, including a *not-installed* one — the first line reads bold
    /// independent of install status (install-ness is carried by the `[installed]`
    /// marker, not the name weight).
    #[test]
    fn double_headline_bold_regardless_of_install() {
        console::set_colors_enabled(true);
        let r = row("aur", "not-here", InstallState::NotInstalled, Some("1.2-1"));
        let line = SearchList {
            rows: std::slice::from_ref(&r),
            numbers: RowNumbers::Plain,
            layout: SearchLayout::Double,
        }
        .render(Paint::Colored, Some(Width::cols(100)))
        .lines()[0]
            .clone();
        let (bold_name, bold_ver) = (
            style("not-here").bold().to_string(),
            style("1.2-1").bold().to_string(),
        );
        assert_contains!(line, bold_name.as_str());
        assert_contains!(line, bold_ver.as_str());
    }

    /// `Auto` renders single-line when the widest row fits the terminal and
    /// flips to two-line when it would overflow — width-adaptive, same rows.
    #[test]
    fn auto_flips_to_double_when_single_line_would_overflow() {
        let mut r = row("aur", "pkg", InstallState::NotInstalled, Some("1-1"));
        r.desc = Some("x".repeat(200)); // a description that can't fit narrow
        let list = |width: Option<Width>| {
            SearchList {
                rows: std::slice::from_ref(&r),
                numbers: RowNumbers::Plain,
                layout: SearchLayout::Auto,
            }
            .render(Paint::Plain, width)
        };
        // Wide terminal: one dense line.
        assert_eq!(list(Some(Width::cols(400))).lines().len(), 1);
        // Narrow terminal: two lines (headline + its own description line).
        assert_eq!(list(Some(Width::cols(40))).lines().len(), 2);
        // A pipe (no width) stays single-line — dense, one record per line.
        assert_eq!(list(None).lines().len(), 1);
    }

    /// `Single` is exactly the aligned grid (one line per row), whatever the
    /// width — the knob overrides `Auto`'s width adaptation.
    #[test]
    fn single_forces_one_line_per_row() {
        let rows = vec![
            row("aur", "a", InstallState::NotInstalled, Some("1-1")),
            row("aur", "b", InstallState::NotInstalled, Some("2-1")),
        ];
        let table = SearchList {
            rows: &rows,
            numbers: RowNumbers::Plain,
            layout: SearchLayout::Single,
        }
        .render(Paint::Plain, Some(Width::cols(10))); // absurdly narrow, still one line each
        assert_eq!(table.lines().len(), 2);
    }
}
