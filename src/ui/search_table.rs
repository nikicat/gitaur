//! The aligned result table for the ranked search list (shell `search` + the
//! non-interactive `aurox <term>` listing), plus the pacman-format `-Ss`
//! block ([`search_result`]).
//!
//! Columns: `repo · name · version · size · build-time · description`, plus a
//! dimmed trailing [`MatchNote`] annotation when the match site is invisible
//! on the row (a hidden split member, a `provides=` name, a group). It
//! renders through the shared [`Grid`] engine and cell vocabulary so the same
//! bugs are fixed once — [`VersionColumn`] for the `old → new` verdiff,
//! [`size_of`](super::cost::size_of)/[`cost_of`](super::cost::cost_of) for the
//! size + build-time cells.
//!
//! Installed packages are set apart by emphasis, not a column (the user's call):
//! an installed row keeps full color with a **bold** name and, when an upgrade
//! is available, an `old → new` diff plus its estimated build time; a
//! not-installed row is dimmed so it recedes. Under `--color=never` the emphasis
//! collapses (there's nothing to dim), but the version/size columns still align.
//!
//! The shell's selector `№` column is part of the row ([`RowNumbers`]), so the
//! number a user types (`add 3`) and the number printed can't drift; the
//! best-last print order stays the shell's job ([`crate::cli::shell`]) — this
//! renders one line per row, in the order given.

use super::cells::VersionColumn;
use super::cost::{PreviewMetrics, RowCost, SizeEst, cost_of, size_of};
use super::grid::{Cell, Col, Grid, GridRow, Paint, Table, Width};
use super::{dim, repo as repo_style};
use crate::names::{GroupName, PkgName, RepoName, VirtualName};
use crate::pacman::alpm_db::PacmanIndex;
use crate::version::Version;
use console::style;
use std::fmt;

/// Whether a searched package is installed locally — and at which version.
///
/// The domain state behind a row's emphasis (installed rows pop, not-installed
/// rows recede), the build-time cell, the table's `old → new` verdiff
/// ([`SearchRow::upgrade_from`]), and the `-Ss` `[installed: X]` marker. The
/// local version travels *inside* the installed variant instead of a sibling
/// field a constructor could leave out of sync.
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
/// version pair against the pacman DBs; the table derives the size + build-time
/// cells from `pac`/`metrics`.
pub struct SearchRow {
    pub repo: RepoName,
    pub name: PkgName,
    /// Installed state, local version included — drives the emphasis, the
    /// build-time cell, and the version diff/marker.
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
}

impl SearchRow {
    /// The installed version when it's behind the available one — the state
    /// behind the table's `old → new` verdiff. A same-version or *newer*
    /// install (e.g. a VCS build ahead of the index) draws no arrow.
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

/// Render the ranked rows into an aligned table — one body line per row.
///
/// Rows come out in the given order with no header (the shell adds one);
/// `numbers` says whether the shell's `№` selector column leads each row.
/// `pac` backs the size cells; `metrics` backs the build-time cells
/// (empty for the non-interactive listing → installed AUR rows show `?`).
/// `paint` is passed in (callers use [`Paint::detect`]) rather than re-read
/// from the environment, so tests pin the plain rendering.
pub fn search_table(
    rows: &[SearchRow],
    pac: &PacmanIndex,
    metrics: &PreviewMetrics,
    numbers: RowNumbers,
    paint: Paint,
) -> Table {
    // Per-row size + cost, computed once (also feeds the column widths).
    let sizes: Vec<SizeEst> = rows
        .iter()
        .map(|r| size_of(&r.repo, &r.name, pac))
        .collect();
    let costs: Vec<RowCost> = rows
        .iter()
        .map(|r| {
            // Build-time is a property we only show for installed packages (the
            // store only has data for things we've built); a not-installed row
            // gets an empty cell rather than a noisy `?`.
            if r.install.installed() {
                cost_of(&r.repo, &r.name, metrics)
            } else {
                RowCost::none()
            }
        })
        .collect();
    let versions =
        VersionColumn::measure(rows.iter().map(|r| (r.upgrade_from(), r.new_ver.as_ref())));

    let mut cols = vec![
        Col::left(),  // repo
        Col::left(),  // name
        Col::left(),  // version block
        Col::left(),  // size (historically left-aligned here; change_set right-aligns)
        Col::right(), // build time
    ];
    if numbers == RowNumbers::Numbered {
        // Floored at three digits — the `{:>3}` the shell's second-pass
        // numbering used to apply — and growing gracefully past row 999.
        cols.insert(0, Col::right().min(Width::of("999")));
    }
    let mut grid = Grid::new(cols);
    for (i, ((row, size), cost)) in rows.iter().zip(&sizes).zip(&costs).enumerate() {
        let em = &row.install;
        let mut cells = vec![
            repo_cell(&row.repo, em, paint),
            name_cell(&row.name, em, paint),
            version_cell(
                &versions,
                em,
                row.upgrade_from(),
                row.new_ver.as_ref(),
                paint,
            ),
            size_cell(*size, em, paint),
            cost.cell(paint),
        ];
        if numbers == RowNumbers::Numbered {
            cells.insert(0, Cell::plain((i + 1).to_string()));
        }
        grid.push(GridRow::new(cells).tail(format!(
            "{}{}",
            desc_cell(row.desc.as_deref(), paint),
            note_cell(row.note.as_ref(), paint),
        )));
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

/// Pacman's install marker, leading space included: ` [installed]`, or
/// ` [installed: X]` when the local version X differs from the listed one —
/// *any* difference, a newer local build included, exactly as `pacman -Ss`
/// decides it. Empty for a not-installed row.
fn marker(row: &SearchRow, paint: Paint) -> String {
    let InstallState::Installed(iv) = &row.install else {
        return String::new();
    };
    let text = if row.new_ver.as_ref().is_some_and(|nv| nv != iv) {
        format!("[installed: {iv}]")
    } else {
        "[installed]".to_owned()
    };
    if paint.colored() {
        format!(" {}", style(text).bold().cyan())
    } else {
        format!(" {text}")
    }
}

/// The repo cell — repo-colored when installed, dimmed (receding) when not.
fn repo_cell(repo: &RepoName, em: &InstallState, paint: Paint) -> Cell {
    Cell::paint(repo.as_str(), paint, |s| {
        if em.installed() {
            repo_style(s).to_string()
        } else {
            dim(s).to_string()
        }
    })
}

/// The name cell — **bold** when installed (it pops), dimmed when not.
fn name_cell(name: &PkgName, em: &InstallState, paint: Paint) -> Cell {
    Cell::paint(name.as_str(), paint, |s| {
        if em.installed() {
            style(s).bold().to_string()
        } else {
            dim(s).to_string()
        }
    })
}

/// The size cell — plain when installed, dimmed when not.
fn size_cell(size: SizeEst, em: &InstallState, paint: Paint) -> Cell {
    Cell::paint(&size.render(), paint, |s| {
        if em.installed() {
            s.to_owned()
        } else {
            dim(s).to_string()
        }
    })
}

/// The version cell, always the full `old_w + → + new_w` block width so the
/// size column lines up across every row:
/// - **upgrade** (`old` present): `old → new` verdiff via the shared
///   [`VersionColumn`], so the coloring matches the transaction table exactly.
/// - **fresh / up-to-date** (`old` is `None`): the available version alone in
///   the `new` slot — default color when installed, dimmed when not (green is
///   reserved for the transaction table's "will install").
fn version_cell(
    versions: &VersionColumn,
    em: &InstallState,
    old: Option<&Version>,
    new: Option<&Version>,
    paint: Paint,
) -> Cell {
    if old.is_some() {
        return versions.cell(old, new, paint);
    }
    let Some(v) = new else {
        return Cell::plain("");
    };
    // The blank old slot + arrow gap keeps fresh rows aligned with upgrades.
    let lead = (versions.old_w + paint.arrow()).blanks();
    let shown = if paint.colored() && !em.installed() {
        dim(v.as_str()).to_string()
    } else {
        v.as_str().to_owned()
    };
    Cell::sized(
        format!("{lead}{shown}"),
        versions.old_w + paint.arrow() + Width::of(v.as_str()),
    )
}

/// The trailing, unaligned description cell — dimmed, with a leading gap; empty
/// when the package has no description.
fn desc_cell(desc: Option<&str>, paint: Paint) -> String {
    match desc {
        Some(d) if !d.is_empty() && paint.colored() => format!("  {}", dim(d)),
        Some(d) if !d.is_empty() => format!("  {d}"),
        _ => String::new(),
    }
}

/// The trailing match-site annotation — dimmed like the description; empty
/// when the match is visible on the row itself.
fn note_cell(note: Option<&MatchNote>, paint: Paint) -> String {
    match note {
        Some(n) if paint.colored() => format!("  {}", dim(n.to_string())),
        Some(n) => format!("  {n}"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert_contains;

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
        }
    }

    /// The plain (un-colored) table: an upgradable installed row shows the
    /// `old -> new` diff, a fresh/up-to-date row shows just the version, the size
    /// cell reaches the table, and descriptions ride along as the trailing column.
    #[test]
    fn plain_table_shows_diff_only_for_upgrades() {
        let mut pac = PacmanIndex::default();
        pac.sync_download_size.insert("clang".into(), 1024);
        pac.installed_size.insert("claude-code".into(), 2048);

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
        let table = search_table(
            &rows,
            &pac,
            &PreviewMetrics::empty(),
            RowNumbers::Plain,
            Paint::Plain,
        );
        let lines = table.lines();
        assert_eq!(lines.len(), 3);

        // Upgrade row carries the arrow; the others don't.
        assert!(
            lines[0].contains("2.0.1-1 -> 2.1.0-1"),
            "row 0: {:?}",
            lines[0]
        );
        assert!(
            !lines[1].contains("->"),
            "fresh row has no arrow: {:?}",
            lines[1]
        );
        assert!(
            !lines[2].contains("->"),
            "up-to-date row has no arrow: {:?}",
            lines[2]
        );

        // Size cell: exact for the repo row, estimated (unmarked) for the
        // installed AUR row.
        assert!(lines[2].contains("1.00 KiB"), "repo size: {:?}", lines[2]);
        assert!(
            lines[0].contains("2.00 KiB"),
            "aur est size: {:?}",
            lines[0]
        );

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
        let table = search_table(
            &rows,
            &PacmanIndex::default(),
            &PreviewMetrics::empty(),
            RowNumbers::Numbered,
            Paint::Plain,
        );
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

    /// A not-installed row shows no build-time cell even when the metrics store
    /// has a figure for that name (build time is an installed-package property).
    #[test]
    fn not_installed_row_omits_build_time() {
        let mut metrics = PreviewMetrics::empty();
        metrics.root_build_secs.insert(PkgName::from("claude"), 200);
        let rows = vec![row(
            RepoName::from("aur"),
            PkgName::from("claude"),
            InstallState::NotInstalled,
            Some(Version::from("1.5.0-1")),
        )];
        let table = search_table(
            &rows,
            &PacmanIndex::default(),
            &metrics,
            RowNumbers::Plain,
            Paint::Plain,
        );
        assert!(
            !table.lines()[0].contains("3m"),
            "not-installed row must not show a build estimate: {:?}",
            table.lines()[0]
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
        let table = search_table(
            &rows,
            &PacmanIndex::default(),
            &PreviewMetrics::empty(),
            RowNumbers::Plain,
            Paint::Plain,
        );
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

    /// An installed AUR row with a recorded build time shows the estimate.
    #[test]
    fn installed_aur_row_shows_build_time() {
        let mut metrics = PreviewMetrics::empty();
        metrics.root_build_secs.insert(PkgName::from("claude"), 200);
        let rows = vec![row(
            RepoName::from("aur"),
            PkgName::from("claude"),
            InstallState::Installed(Version::from("1.5.0-1")),
            Some(Version::from("1.5.0-1")),
        )];
        let table = search_table(
            &rows,
            &PacmanIndex::default(),
            &metrics,
            RowNumbers::Plain,
            Paint::Plain,
        );
        assert!(
            table.lines()[0].contains("3m 20s"),
            "installed AUR row shows its build estimate: {:?}",
            table.lines()[0]
        );
    }
}
