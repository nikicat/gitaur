//! The aligned result table for the ranked search list (shell `search` + the
//! non-interactive `aurox <term>` listing).
//!
//! Columns: `repo · name · version · size · build-time · description`. It
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
use crate::names::{PkgName, RepoName};
use crate::pacman::alpm_db::PacmanIndex;
use crate::version::Version;
use console::style;

/// Whether a searched package is installed locally.
///
/// The domain state behind a row's emphasis (installed rows pop, not-installed
/// rows recede) and whether the build-time cell is shown — a named two-state
/// instead of a bare `bool`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallState {
    Installed,
    NotInstalled,
}

impl InstallState {
    /// Lift a `pac.is_installed(name)` answer into the domain state.
    pub const fn from_installed(installed: bool) -> Self {
        if installed {
            Self::Installed
        } else {
            Self::NotInstalled
        }
    }

    pub const fn installed(self) -> bool {
        matches!(self, Self::Installed)
    }
}

/// One search hit, ready to render. The caller resolves installed state and the
/// version pair against the pacman DBs; the table derives the size + build-time
/// cells from `pac`/`metrics`.
pub struct SearchRow {
    pub repo: RepoName,
    pub name: PkgName,
    /// Whether the package is installed — drives the emphasis and whether the
    /// build-time cell is shown.
    pub install: InstallState,
    /// The installed version, set **only** when it's an upgrade (installed <
    /// available) so the renderer draws `old → new`; `None` for a fresh or
    /// up-to-date row.
    pub old_ver: Option<Version>,
    /// The available version (repo/AUR); `None` only when it couldn't be looked
    /// up (the version cell then renders blank but aligned).
    pub new_ver: Option<Version>,
    /// The one-line package description, shown dimmed as the trailing column.
    pub desc: Option<String>,
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
    let versions = VersionColumn::measure(
        rows.iter()
            .map(|r| (r.old_ver.as_ref(), r.new_ver.as_ref())),
    );

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
        let em = row.install;
        let mut cells = vec![
            repo_cell(&row.repo, em, paint),
            name_cell(&row.name, em, paint),
            version_cell(
                &versions,
                em,
                row.old_ver.as_ref(),
                row.new_ver.as_ref(),
                paint,
            ),
            size_cell(*size, em, paint),
            cost.cell(paint),
        ];
        if numbers == RowNumbers::Numbered {
            cells.insert(0, Cell::plain((i + 1).to_string()));
        }
        grid.push(GridRow::new(cells).tail(desc_cell(row.desc.as_deref(), paint)));
    }
    grid.render()
}

/// The repo cell — repo-colored when installed, dimmed (receding) when not.
fn repo_cell(repo: &RepoName, em: InstallState, paint: Paint) -> Cell {
    Cell::paint(repo.as_str(), paint, |s| {
        if em.installed() {
            repo_style(s).to_string()
        } else {
            dim(s).to_string()
        }
    })
}

/// The name cell — **bold** when installed (it pops), dimmed when not.
fn name_cell(name: &PkgName, em: InstallState, paint: Paint) -> Cell {
    Cell::paint(name.as_str(), paint, |s| {
        if em.installed() {
            style(s).bold().to_string()
        } else {
            dim(s).to_string()
        }
    })
}

/// The size cell — plain when installed, dimmed when not.
fn size_cell(size: SizeEst, em: InstallState, paint: Paint) -> Cell {
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
    em: InstallState,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a row from domain-typed parts, deriving a description from the
    /// name so the trailing column has something to show.
    fn row(
        repo: RepoName,
        name: PkgName,
        install: InstallState,
        old: Option<Version>,
        new: Option<Version>,
    ) -> SearchRow {
        let desc = Some(format!("{} description", name.as_str()));
        SearchRow {
            repo,
            name,
            install,
            old_ver: old,
            new_ver: new,
            desc,
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
                InstallState::Installed,
                Some(Version::from("2.0.1-1")),
                Some(Version::from("2.1.0-1")),
            ),
            row(
                RepoName::from("aur"),
                PkgName::from("claude"),
                InstallState::NotInstalled,
                None,
                Some(Version::from("1.5.0-1")),
            ),
            row(
                RepoName::from("extra"),
                PkgName::from("clang"),
                InstallState::Installed,
                None,
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
                None,
                Some(Version::from("1-1")),
            ),
            row(
                RepoName::from("aur"),
                PkgName::from("second"),
                InstallState::NotInstalled,
                None,
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
            None,
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

    /// An installed AUR row with a recorded build time shows the estimate.
    #[test]
    fn installed_aur_row_shows_build_time() {
        let mut metrics = PreviewMetrics::empty();
        metrics.root_build_secs.insert(PkgName::from("claude"), 200);
        let rows = vec![row(
            RepoName::from("aur"),
            PkgName::from("claude"),
            InstallState::Installed,
            None,
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
