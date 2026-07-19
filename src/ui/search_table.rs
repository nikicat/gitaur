//! The aligned result table for the ranked search list (shell `search` + the
//! non-interactive `aurox <term>` listing), plus the pacman-format `-Ss`
//! block ([`search_result`]).
//!
//! Columns: `repo · name · version · installed · freshness`, then the
//! description (and a dimmed trailing [`MatchNote`] annotation when the match
//! site is invisible on the row — a hidden split member, a `provides=` name, a
//! group) as the unaligned tail. Everything fixed-width is an aligned column so
//! the descriptions line up across rows; only the free-text description +
//! match-note ride in the tail. Cost signals (download size, build time) are
//! deliberately absent — they matter at commit time, so they live in the
//! `apply` change-set preview, not the search scan. Renders through the shared
//! [`Grid`] engine and cell vocabulary.
//!
//! Color is **additive**, not subtractive: every row carries the baseline —
//! the repo in its hashed color, a readable name and (available) version — so a
//! not-installed row (the common search hit) reads clearly instead of receding
//! into gray. The primary identity cells (repo, name, version) get color; the
//! trailing description / match-note dim. Installed-ness is a **positive**
//! accent that survives a colorful table: a **bold** name plus the
//! **installed-version** column — the local version you have, dimmed when it
//! matches the available one and **yellow** when it's behind (an upgrade
//! waits). AUR rows also carry a freshness column — a coarse age (`3d`) colored
//! by risk band (see [`super::freshness`]); it's its own column, so no brackets
//! delimit it. Under `--color=never` the accents collapse to their plain forms
//! (bold name, the bare installed version, the bare age), and every column
//! still aligns.
//!
//! The shell's selector `№` column is part of the row ([`RowNumbers`]), so the
//! number a user types (`add 3`) and the number printed can't drift; the
//! best-last print order stays the shell's job ([`crate::cli::shell`]) — this
//! renders one line per row, in the order given.

use super::freshness::Freshness;
use super::grid::{Cell, Col, Grid, GridRow, Paint, Table, Width};
use super::{dim, repo as repo_style};
use crate::names::{GroupName, PkgName, RepoName, VirtualName};
use crate::version::Version;
use console::style;
use std::fmt;

/// Whether a searched package is installed locally — and at which version.
///
/// The domain state behind a row's name emphasis (installed rows pop,
/// not-installed rows recede), the installed-version column
/// ([`SearchRow::upgrade_from`] decides its "behind" styling), and the `-Ss`
/// `[installed: X]` marker. The local version travels *inside* the installed
/// variant instead of a sibling field a constructor could leave out of sync.
#[derive(Debug, Clone, PartialEq)]
pub enum InstallState {
    /// Installed at this local version (the pacman localdb's answer).
    Installed(Version),
    NotInstalled,
}

impl InstallState {
    pub const fn installed(&self) -> bool {
        matches!(self, Self::Installed(_))
    }
}

/// Why a row matched when nothing visible on it shows the term — rendered as
/// the dimmed trailing annotation after the description.
///
/// The bracket wording matches the review header's `[provides {name}]` /
/// `[replaces {name}]` vocabulary ([`crate::build::review`]) — a display
/// convention, not shared code (different data feeds each).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchNote {
    /// A split member's pkgname (or its member-only description) matched —
    /// the displayed pkgbase name doesn't show it.
    Via(PkgName),
    /// A `provides=` name matched (bare, constraint-stripped).
    Provides(VirtualName),
    /// A pacman group matched (repo rows only).
    Group(GroupName),
}

impl fmt::Display for MatchNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Via(n) => write!(f, "[via {n}]"),
            Self::Provides(v) => write!(f, "[provides {v}]"),
            Self::Group(g) => write!(f, "[group {g}]"),
        }
    }
}

/// One search hit, ready to render. The caller resolves installed state and the
/// available/installed version pair against the pacman DBs.
pub struct SearchRow {
    pub repo: RepoName,
    pub name: PkgName,
    /// Installed state, local version included — drives the name emphasis and
    /// the installed-version column (its value and its styled-when-behind
    /// coloring).
    pub install: InstallState,
    /// The available version (repo/AUR); `None` only when it couldn't be looked
    /// up (the version cell then renders blank but aligned).
    pub new_ver: Option<Version>,
    /// The one-line package description, shown dimmed as the trailing column.
    pub desc: Option<String>,
    /// Why the row matched when the term is invisible on it (member pkgname,
    /// provides, group); `None` when the visible name/description explains
    /// the match.
    pub note: Option<MatchNote>,
    /// The AUR freshness band badge (last-change age → risk band), rendered as
    /// the freshness column (a coarse age like `3d`). `None` for repo rows, the
    /// `-Ss` surface, and AUR entries with an unknown commit time. See the
    /// `ui::freshness` module.
    pub freshness: Option<Freshness>,
}

impl SearchRow {
    /// The installed version when it's behind the available one — the state
    /// behind the installed-version column's "behind" (yellow) styling. A
    /// same-version or *newer* install (e.g. a VCS build ahead of the index)
    /// is up to date, so it styles as current.
    pub fn upgrade_from(&self) -> Option<&Version> {
        let InstallState::Installed(iv) = &self.install else {
            return None;
        };
        let nv = self.new_ver.as_ref()?;
        iv.is_outdated(nv.as_ver()).then_some(iv)
    }
}

/// Whether the table carries the shell's selector row numbers as its first
/// column.
///
/// The shell renders [`Self::Numbered`] — each row's `№` is the index the
/// selector verbs (`add 3`) resolve against, so the number is part of the row,
/// not a second layout pass bolted on top. The non-interactive pipe listing
/// renders [`Self::Plain`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowNumbers {
    Numbered,
    Plain,
}

/// Render the ranked rows into the aligned single-line table — one body line
/// per row.
///
/// The single-line layout behind [`super::SearchList`] (the layout dispatcher
/// picks it or the two-line form per [`super::SearchLayout`]); not called
/// directly — surfaces go through `SearchList`. Rows come out in the given
/// order with no header (the shell adds one); `numbers` says whether the
/// shell's `№` selector column leads each row. `paint` is passed in (callers
/// use [`Paint::detect`]) rather than re-read from the environment, so tests
/// pin the plain rendering.
pub(super) fn search_table(rows: &[SearchRow], numbers: RowNumbers, paint: Paint) -> Table {
    let mut cols = vec![
        Col::left(), // repo
        Col::left(), // name
        Col::left(), // version (available)
        Col::left(), // installed version (local; styled when behind available)
        Col::left(), // freshness band
    ];
    if numbers == RowNumbers::Numbered {
        // Floored at three digits — the `{:>3}` the shell's second-pass
        // numbering used to apply — and growing gracefully past row 999.
        cols.insert(0, Col::right().min(Width::of("999")));
    }
    let mut grid = Grid::new(cols);
    for (i, row) in rows.iter().enumerate() {
        let em = &row.install;
        let mut cells = vec![
            repo_cell(&row.repo, paint),
            name_cell(&row.name, em, paint),
            version_cell(row.new_ver.as_ref()),
            installed_cell(row, paint),
            freshness_cell(row.freshness, paint),
        ];
        if numbers == RowNumbers::Numbered {
            cells.insert(0, Cell::plain((i + 1).to_string()));
        }
        // Only the description and its match-note ride in the tail — every
        // fixed-width field is an aligned column, so descriptions line up. Cost
        // signals (download size, build time) are deliberately *not* here: they
        // matter when you commit a transaction, so they live in the `apply`
        // change-set preview, not the search scan.
        grid.push(GridRow::new(cells).tail(vec![
            desc_cell(row.desc.as_deref(), paint),
            note_cell(row.note.as_ref(), paint),
        ]));
    }
    grid.render()
}

/// Render one hit in pacman's `-Ss` layout.
///
/// The `repo/name version` headline (the ` [installed]` / ` [installed: X]`
/// marker appended pacman-style) and the indented description line, omitted
/// when the source has none.
///
/// Colored paint follows pacman's own `-Ss` palette — bold name, bold-green
/// version, bold-cyan `[installed]`, plain description — except the repo,
/// which keeps the hash color the aligned table uses ([`super::repo`]), so a
/// repo wears one color across every search surface. Plain paint renders the
/// exact pacman byte layout.
pub fn search_result(row: &SearchRow, paint: Paint) -> Table {
    let mut out = Table::new();
    out.push(headline(row, paint));
    if let Some(desc) = row.desc.as_deref() {
        out.push(format!("    {desc}"));
    }
    out
}

/// The `-Ss` headline line — see [`search_result`] for the palette.
fn headline(row: &SearchRow, paint: Paint) -> String {
    let repo = row.repo.as_str();
    let name = row.name.as_str();
    let ver = row.new_ver.as_ref().map_or("", |v| v.as_str());
    let marker = marker(row, paint);
    if paint.colored() {
        format!(
            "{}/{} {}{marker}",
            repo_style(repo),
            style(name).bold(),
            style(ver).bold().green(),
        )
    } else {
        format!("{repo}/{name} {ver}{marker}")
    }
}

/// The pacman install-marker *text* — no color, no leading space:
/// `[installed]`, or `[installed: X]` when the local version X differs from the
/// listed one (*any* difference, a newer local build included, exactly as
/// `pacman -Ss` decides it). `None` for a not-installed row.
///
/// The one place the marker's shape is decided, shared by the `-Ss` headline
/// ([`marker`], bold cyan) and the two-line interactive headline
/// ([`super::SearchList`], currency-colored) so the two can't drift.
pub(super) fn installed_marker_text(row: &SearchRow) -> Option<String> {
    let InstallState::Installed(iv) = &row.install else {
        return None;
    };
    Some(if row.new_ver.as_ref().is_some_and(|nv| nv != iv) {
        format!("[installed: {iv}]")
    } else {
        "[installed]".to_owned()
    })
}

/// Pacman's `-Ss` install marker, leading space included: ` [installed]` /
/// ` [installed: X]` in bold cyan (pacman's palette). Empty for a
/// not-installed row. The text comes from [`installed_marker_text`].
fn marker(row: &SearchRow, paint: Paint) -> String {
    let Some(text) = installed_marker_text(row) else {
        return String::new();
    };
    if paint.colored() {
        format!(" {}", style(text).bold().cyan())
    } else {
        format!(" {text}")
    }
}

/// The repo cell — always in the repo's hashed color (provenance is a per-row
/// baseline signal, shown whether or not the package is installed).
fn repo_cell(repo: &RepoName, paint: Paint) -> Cell {
    Cell::paint(repo.as_str(), paint, |s| repo_style(s).to_string())
}

/// The name cell — the primary identifier, always readable: **bold** when
/// installed (it pops), plain terminal foreground otherwise (not dimmed).
fn name_cell(name: &PkgName, em: &InstallState, paint: Paint) -> Cell {
    Cell::paint(name.as_str(), paint, |s| {
        if em.installed() {
            style(s).bold().to_string()
        } else {
            s.to_owned()
        }
    })
}

/// The installed-version column: the local version when the package is
/// installed, styled by whether it's current — **dimmed** when up to date (you
/// already have the available version) and **yellow** when it's behind the
/// available one (an upgrade waits). Empty (the grid collapses it) when the
/// package isn't installed. This carries the whole installed signal now: which
/// version you have, and whether it's the latest.
fn installed_cell(row: &SearchRow, paint: Paint) -> Cell {
    let InstallState::Installed(iv) = &row.install else {
        return Cell::plain("");
    };
    let outdated = row.upgrade_from().is_some();
    Cell::paint(iv.as_str(), paint, |s| {
        if outdated {
            style(s).yellow().to_string()
        } else {
            dim(s).to_string()
        }
    })
}

/// The freshness-band column (a coarse age like `3d`, colored by risk band), or
/// an empty cell (grid-collapsed) for a repo row / an AUR row with an unknown
/// commit time.
fn freshness_cell(freshness: Option<Freshness>, paint: Paint) -> Cell {
    freshness.map_or_else(|| Cell::plain(""), |f| f.cell(paint))
}

/// The available-version column — the version a pick would install, in plain
/// terminal foreground on every row (green stays reserved for the transaction
/// table's "will install"; the installed-vs-available comparison lives in the
/// adjacent [`installed_cell`]). Blank when the version couldn't be looked up.
fn version_cell(new: Option<&Version>) -> Cell {
    Cell::plain(new.map_or("", |v| v.as_str()))
}

/// The trailing description tail cell — dimmed; an empty cell (grid-skipped)
/// when the package has no description.
fn desc_cell(desc: Option<&str>, paint: Paint) -> Cell {
    match desc {
        Some(d) if !d.is_empty() => Cell::paint(d, paint, |s| dim(s).to_string()),
        _ => Cell::plain(""),
    }
}

/// The trailing match-site annotation tail cell — dimmed like the description;
/// an empty cell when the match is visible on the row itself.
fn note_cell(note: Option<&MatchNote>, paint: Paint) -> Cell {
    match note {
        Some(n) => {
            let text = n.to_string();
            Cell::paint(&text, paint, |s| dim(s).to_string())
        }
        None => Cell::plain(""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{assert_contains, assert_not_contains};

    /// Assemble a row from domain-typed parts, deriving a description from the
    /// name so the trailing column has something to show.
    fn row(
        repo: RepoName,
        name: PkgName,
        install: InstallState,
        new: Option<Version>,
    ) -> SearchRow {
        let desc = Some(format!("{} description", name.as_str()));
        SearchRow {
            repo,
            name,
            install,
            new_ver: new,
            desc,
            note: None,
            freshness: None,
        }
    }

    /// The plain (un-colored) table: the version column shows the available
    /// version and the installed column shows the local version only when
    /// installed — separate columns, no inline `->` diff — and descriptions
    /// ride the tail.
    #[test]
    fn plain_table_splits_available_and_installed_versions() {
        let rows = vec![
            row(
                RepoName::from("aur"),
                PkgName::from("claude-code"),
                InstallState::Installed(Version::from("2.0.1-1")),
                Some(Version::from("2.1.0-1")),
            ),
            row(
                RepoName::from("aur"),
                PkgName::from("claude"),
                InstallState::NotInstalled,
                Some(Version::from("1.5.0-1")),
            ),
            row(
                RepoName::from("extra"),
                PkgName::from("clang"),
                InstallState::Installed(Version::from("18.1.0-1")),
                Some(Version::from("18.1.0-1")),
            ),
        ];
        let table = search_table(&rows, RowNumbers::Plain, Paint::Plain);
        let lines = table.lines();
        assert_eq!(lines.len(), 3);

        // No inline diff arrow anywhere — the two versions are separate columns.
        for line in lines {
            assert!(!line.contains("->"), "no arrow: {line:?}");
        }
        // Behind row (installed 2.0.1-1, available 2.1.0-1): both versions show.
        assert!(lines[0].contains("2.1.0-1"), "available: {:?}", lines[0]);
        assert!(lines[0].contains("2.0.1-1"), "installed: {:?}", lines[0]);
        // Not-installed row: the available version, nothing in the installed col.
        assert!(lines[1].contains("1.5.0-1"), "{:?}", lines[1]);
        // Current row: available == installed.
        assert!(lines[2].contains("18.1.0-1"), "{:?}", lines[2]);

        assert!(lines[1].contains("claude description"));
    }

    /// `Numbered` leads each row with its 1-based selector index, right-aligned
    /// in a column floored at three digits — byte-identical to the `{:>3}  `
    /// prefix the shell used to bolt on in a second pass.
    #[test]
    fn numbered_rows_carry_the_selector_index() {
        let rows = vec![
            row(
                RepoName::from("aur"),
                PkgName::from("first"),
                InstallState::NotInstalled,
                Some(Version::from("1-1")),
            ),
            row(
                RepoName::from("aur"),
                PkgName::from("second"),
                InstallState::NotInstalled,
                Some(Version::from("2-1")),
            ),
        ];
        let table = search_table(&rows, RowNumbers::Numbered, Paint::Plain);
        assert!(
            table.lines()[0].starts_with("  1  aur"),
            "row 1: {:?}",
            table.lines()[0]
        );
        assert!(
            table.lines()[1].starts_with("  2  aur"),
            "row 2: {:?}",
            table.lines()[1]
        );
    }

    /// The match-site annotation renders as the trailing `[...]` cell after
    /// the description — one wording per variant — and only when present.
    #[test]
    fn note_renders_after_desc_and_only_when_present() {
        let mut provides = row(
            RepoName::from("aur"),
            PkgName::from("linux-zz"),
            InstallState::NotInstalled,
            Some(Version::from("1-1")),
        );
        provides.note = Some(MatchNote::Provides(VirtualName::new(
            "VIRTUALBOX-GUEST-MODULES",
        )));
        let mut via = row(
            RepoName::from("aur"),
            PkgName::from("openrc-misc"),
            InstallState::NotInstalled,
            Some(Version::from("1-1")),
        );
        via.note = Some(MatchNote::Via(PkgName::from(
            "virtualbox-guest-utils-openrc",
        )));
        let mut group = row(
            RepoName::from("extra"),
            PkgName::from("qemu-zz"),
            InstallState::NotInstalled,
            Some(Version::from("1-1")),
        );
        group.note = Some(MatchNote::Group(GroupName::new("virt-tools")));
        let plain = row(
            RepoName::from("aur"),
            PkgName::from("claude"),
            InstallState::NotInstalled,
            Some(Version::from("1-1")),
        );

        let rows = vec![provides, via, group, plain];
        let table = search_table(&rows, RowNumbers::Plain, Paint::Plain);
        let lines = table.lines();
        assert!(
            lines[0].ends_with("linux-zz description  [provides VIRTUALBOX-GUEST-MODULES]"),
            "provides note trails the desc: {:?}",
            lines[0]
        );
        assert!(
            lines[1].ends_with("  [via virtualbox-guest-utils-openrc]"),
            "via note: {:?}",
            lines[1]
        );
        assert!(
            lines[2].ends_with("  [group virt-tools]"),
            "group note: {:?}",
            lines[2]
        );
        assert!(
            !lines[3].contains('['),
            "no note, no bracket: {:?}",
            lines[3]
        );
    }

    /// The colored `-Ss` block actually carries ANSI styling on the headline
    /// (the regression that motivated it: `-Ss` printed plain bytes on a color
    /// terminal), strips back to the exact plain bytes, and leaves the
    /// description line plain like pacman does.
    #[test]
    fn search_result_colored_strips_to_plain() {
        // `console` gates styling on its own stdout-TTY detection at render
        // time; force it on so the colored branch is observable when the test
        // runs piped (plain `cargo test`), not only under makepkg's tty.
        console::set_colors_enabled(true);
        let r = SearchRow {
            repo: RepoName::from("extra"),
            name: PkgName::from("qemu-desktop"),
            install: InstallState::Installed(Version::from("11.0.2-3")),
            new_ver: Some(Version::from("11.0.2-3")),
            desc: Some("A QEMU setup for desktop environments".into()),
            note: None,
            freshness: None,
        };
        let plain = search_result(&r, Paint::Plain);
        let colored = search_result(&r, Paint::Colored);
        assert_eq!(plain.lines().len(), 2);
        assert_eq!(plain.lines()[0], "extra/qemu-desktop 11.0.2-3 [installed]");
        assert_contains!(colored.lines()[0], "\u{1b}[");
        assert_eq!(
            console::strip_ansi_codes(&colored.lines()[0]),
            plain.lines()[0],
            "colored headline must strip to the plain bytes"
        );
        assert_eq!(
            colored.lines()[1],
            plain.lines()[1],
            "description line stays plain (pacman parity)"
        );
    }

    /// An installed row whose local version differs from the listed one carries
    /// pacman's `[installed: X]` marker — shown for *any* difference, a newer
    /// local build included — and the colored form strips to the same bytes.
    #[test]
    fn search_result_marks_version_drift() {
        console::set_colors_enabled(true);
        let mut r = SearchRow {
            repo: RepoName::from("extra"),
            name: PkgName::from("qemu-desktop"),
            install: InstallState::Installed(Version::from("11.0.1-2")),
            new_ver: Some(Version::from("11.0.2-3")),
            desc: None,
            note: None,
            freshness: None,
        };
        let plain = search_result(&r, Paint::Plain);
        assert_eq!(
            plain.lines()[0],
            "extra/qemu-desktop 11.0.2-3 [installed: 11.0.1-2]"
        );
        let colored = search_result(&r, Paint::Colored);
        assert_eq!(
            console::strip_ansi_codes(&colored.lines()[0]),
            plain.lines()[0]
        );

        // Newer-than-listed (a VCS build ahead of the index) still drifts.
        r.install = InstallState::Installed(Version::from("11.0.3-1"));
        assert_eq!(
            search_result(&r, Paint::Plain).lines()[0],
            "extra/qemu-desktop 11.0.2-3 [installed: 11.0.3-1]"
        );
    }

    /// A not-installed row has no marker and renders one headline line when the
    /// source has no description — in both paints.
    #[test]
    fn search_result_omits_marker_and_desc() {
        console::set_colors_enabled(true);
        let r = SearchRow {
            repo: RepoName::from("aur"),
            name: PkgName::from("qemu-rutabaga"),
            install: InstallState::NotInstalled,
            new_ver: Some(Version::from("9.2.3-1")),
            desc: None,
            note: None,
            freshness: None,
        };
        for paint in [Paint::Plain, Paint::Colored] {
            let table = search_result(&r, paint);
            assert_eq!(table.lines().len(), 1, "no desc line under {paint:?}");
            assert_eq!(
                console::strip_ansi_codes(&table.lines()[0]),
                "aur/qemu-rutabaga 9.2.3-1"
            );
        }
    }

    /// A freshness badge `days_old` days old, against a fixed clock — for the
    /// tail-rendering tests.
    fn badge(days_old: i64) -> Option<Freshness> {
        use crate::ui::{AgeScale, AgeThresholds};
        use crate::units::UnixTime;
        let sec_per_day = 86_400;
        let now = UnixTime::new(1_000 * sec_per_day).system_time()?;
        let scale = AgeScale::at(now, AgeThresholds::from_days(2, 180, 730));
        scale.badge(UnixTime::new((1_000 - days_old) * sec_per_day), false)
    }

    /// Additive coloring: a *not-installed* AUR row still carries color — the
    /// repo in its hashed color, the name in plain foreground (not the dim
    /// italic the old subtractive scheme applied) — while the secondary
    /// description dims. Pins that the "dim monochrome" regression is gone.
    #[test]
    fn not_installed_row_is_colored_not_dimmed() {
        console::set_colors_enabled(true);
        let rows = vec![row(
            RepoName::from("aur"),
            PkgName::from("ripgrep-git"),
            InstallState::NotInstalled,
            Some(Version::from("14.1.0-1")),
        )];
        let table = search_table(&rows, RowNumbers::Plain, Paint::Colored);
        let line = &table.lines()[0];
        let (repo_col, dim_name, dim_desc) = (
            repo_style("aur").to_string(),
            dim("ripgrep-git").to_string(),
            dim("ripgrep-git description").to_string(),
        );
        // The repo wears its hashed color (baseline provenance), not dimmed away.
        assert_contains!(line, repo_col.as_str());
        // The name is NOT wrapped in the dim italic sequence.
        assert_not_contains!(line, dim_name.as_str());
        // The description is still dimmed (a secondary cell).
        assert_contains!(line, dim_desc.as_str());
    }

    /// An installed, up-to-date row shows its local version in the installed
    /// column (not a `[installed]` text marker) and, for AUR, its freshness age
    /// — both aligned columns ahead of the description.
    #[test]
    fn installed_up_to_date_row_shows_version_and_freshness() {
        let mut r = row(
            RepoName::from("aur"),
            PkgName::from("claude"),
            InstallState::Installed(Version::from("1.5.0-1")),
            Some(Version::from("1.5.0-1")),
        );
        r.freshness = badge(3);
        let table = search_table(&[r], RowNumbers::Plain, Paint::Plain);
        let line = &table.lines()[0];
        assert_not_contains!(line, "[installed]");
        assert_contains!(line, "1.5.0-1"); // the installed (and available) version
        let tag = line.find("3d").expect("has freshness age");
        let desc = line.find("claude description").expect("has desc");
        assert!(
            tag < desc,
            "freshness column precedes the description: {line:?}"
        );
    }

    /// An *upgradable* installed row shows the available version and, in the
    /// installed column, the local version it's behind — no inline `->` diff,
    /// no `[installed]` text marker.
    #[test]
    fn upgradable_row_splits_versions() {
        let rows = vec![row(
            RepoName::from("aur"),
            PkgName::from("claude"),
            InstallState::Installed(Version::from("1.5.0-1")),
            Some(Version::from("1.6.0-1")),
        )];
        let line = search_table(&rows, RowNumbers::Plain, Paint::Plain).lines()[0].clone();
        assert_not_contains!(line, "->");
        assert_not_contains!(line, "[installed]");
        assert_contains!(line, "1.6.0-1"); // available
        assert_contains!(line, "1.5.0-1"); // installed, behind
    }

    /// The installed version is styled by currency: dimmed when it matches the
    /// available version, yellow when it's behind (an upgrade waits).
    #[test]
    fn installed_version_styled_by_currency() {
        console::set_colors_enabled(true);
        let render = |installed: &str, available: &str| {
            let rows = vec![row(
                RepoName::from("extra"),
                PkgName::from("clang"),
                InstallState::Installed(Version::from(installed)),
                Some(Version::from(available)),
            )];
            search_table(&rows, RowNumbers::Plain, Paint::Colored).lines()[0].clone()
        };
        let current = render("18.1.0-1", "18.1.0-1");
        let behind = render("18.0.0-1", "18.1.0-1");
        let (dim_cur, yellow_behind) = (
            dim("18.1.0-1").to_string(),
            style("18.0.0-1").yellow().to_string(),
        );
        assert_contains!(current, dim_cur.as_str());
        assert_contains!(behind, yellow_behind.as_str());
    }

    /// A not-installed AUR row shows its freshness age but has nothing in the
    /// installed column.
    #[test]
    fn not_installed_row_shows_freshness_without_installed_version() {
        let mut r = row(
            RepoName::from("aur"),
            PkgName::from("some-old-pkg"),
            InstallState::NotInstalled,
            Some(Version::from("1.0-1")),
        );
        r.freshness = badge(900); // > 730d → stale band
        let line = search_table(&[r], RowNumbers::Plain, Paint::Plain).lines()[0].clone();
        assert_contains!(line, "900d");
        assert_not_contains!(line, "[installed]");
    }
}
