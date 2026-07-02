//! The aligned result table for the ranked search list (shell `search` + the
//! non-interactive `gaur <term>` listing).
//!
//! Columns: `repo · name · version · size · build-time · description`. It shares
//! the change-set/upgrade table's cell machinery so the same bugs are fixed
//! once — [`version_block`](super::tables::version_block) for the `old → new`
//! verdiff, [`size_of`](super::cost::size_of)/[`cost_of`](super::cost::cost_of)
//! for the size + build-time cells, and [`Width`]/[`Cell`] for the padding.
//!
//! Installed packages are set apart by emphasis, not a column (the user's call):
//! an installed row keeps full color with a **bold** name and, when an upgrade
//! is available, an `old → new` diff plus its estimated build time; a
//! not-installed row is dimmed so it recedes. Under `--color=never` the emphasis
//! collapses (there's nothing to dim), but the version/size columns still align.
//!
//! The row *number* and best-last print order are the shell's job
//! ([`crate::cli::shell`]); this renders bodies only, one line per row, in the
//! order given.

use super::cost::{PreviewMetrics, RowCost, SizeEst, cost_of, size_of, time_col};
use super::tables::{ARROW, Cell, Paint, Table, Width, version_block};
use super::{color_on, dim, repo as repo_style};
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

/// Render the ranked rows into an aligned table — one body line per row.
///
/// Rows come out in the given order, with no number and no header (the shell
/// adds those). `pac` backs the size cells; `metrics` backs the build-time cells
/// (empty for the non-interactive listing → installed AUR rows show `~?`).
pub fn search_table(rows: &[SearchRow], pac: &PacmanIndex, metrics: &PreviewMetrics) -> Table {
    let paint = Paint::from(color_on());

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
            // gets an empty cell rather than a noisy `~?`.
            if r.install.installed() {
                cost_of(&r.repo, &r.name, metrics)
            } else {
                RowCost::none()
            }
        })
        .collect();

    let repo_w = Width::widest(rows.iter().map(|r| Width::of(r.repo.as_str())));
    let name_w = Width::widest(rows.iter().map(|r| Width::of(r.name.as_str())));
    let old_w = Width::widest(
        rows.iter()
            .filter_map(|r| r.old_ver.as_ref())
            .map(|v| Width::of(v.as_str())),
    );
    let new_w = Width::widest(
        rows.iter()
            .filter_map(|r| r.new_ver.as_ref())
            .map(|v| Width::of(v.as_str())),
    );
    let size_w = Width::widest(sizes.iter().map(|s| Width::of(&s.render())));
    let time_w = Width::widest(costs.iter().map(|c| c.visible_width()));

    let mut out = Table::new();
    for ((row, size), cost) in rows.iter().zip(&sizes).zip(&costs) {
        let em = row.install;
        let repo_cell = repo_cell(&row.repo, em, paint).pad_to(repo_w);
        let name_cell = name_cell(&row.name, em, paint).pad_to(name_w);
        let ver = version_cell(
            em,
            row.old_ver.as_ref(),
            row.new_ver.as_ref(),
            old_w,
            new_w,
            paint,
        );
        let size_cell = size_cell(*size, em, paint).pad_to(size_w);
        out.push(format!(
            "{repo_cell}  {name_cell}  {ver}  {size_cell}  {time}{desc}",
            time = time_col(*cost, time_w, paint),
            desc = desc_cell(row.desc.as_deref(), paint),
        ));
    }
    out
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

/// The size cell (padded to the size column at the call site) — plain when
/// installed, dimmed when not.
fn size_cell(size: SizeEst, em: InstallState, paint: Paint) -> Cell {
    Cell::paint(&size.render(), paint, |s| {
        if em.installed() {
            s.to_owned()
        } else {
            dim(s).to_string()
        }
    })
}

/// The version cell, padded to the full `old_w + → + new_w` block width so the
/// size column lines up across every row:
/// - **upgrade** (`old` present): `old → new` verdiff via the shared
///   [`version_block`], so the coloring matches the upgrade table exactly.
/// - **fresh / up-to-date** (`old` is `None`): the available version alone in
///   the `new` slot — default color when installed, dimmed when not (green is
///   reserved for the transaction table's "will install").
fn version_cell(
    em: InstallState,
    old: Option<&Version>,
    new: Option<&Version>,
    old_w: Width,
    new_w: Width,
    paint: Paint,
) -> String {
    if old.is_some() {
        return version_block(old, new, old_w, new_w, paint);
    }
    let full = old_w + ARROW + new_w;
    let Some(v) = new else {
        return full.blanks();
    };
    let lead = (old_w + ARROW).blanks();
    let pad = new_w.gap(Width::of(v.as_str()));
    let shown = if paint.colored() && !em.installed() {
        dim(v.as_str()).to_string()
    } else {
        v.as_str().to_owned()
    };
    format!("{lead}{shown}{pad}")
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
        super::super::set_color(super::super::ColorMode::Never);
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
        let table = search_table(&rows, &pac, &PreviewMetrics::empty());
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

    /// A not-installed row shows no build-time cell even when the metrics store
    /// has a figure for that name (build time is an installed-package property).
    #[test]
    fn not_installed_row_omits_build_time() {
        super::super::set_color(super::super::ColorMode::Never);
        let mut metrics = PreviewMetrics::empty();
        metrics.root_build_secs.insert(PkgName::from("claude"), 200);
        let rows = vec![row(
            RepoName::from("aur"),
            PkgName::from("claude"),
            InstallState::NotInstalled,
            None,
            Some(Version::from("1.5.0-1")),
        )];
        let table = search_table(&rows, &PacmanIndex::default(), &metrics);
        assert!(
            !table.lines()[0].contains("3m"),
            "not-installed row must not show a build estimate: {:?}",
            table.lines()[0]
        );
    }

    /// An installed AUR row with a recorded build time shows the estimate.
    #[test]
    fn installed_aur_row_shows_build_time() {
        super::super::set_color(super::super::ColorMode::Never);
        let mut metrics = PreviewMetrics::empty();
        metrics.root_build_secs.insert(PkgName::from("claude"), 200);
        let rows = vec![row(
            RepoName::from("aur"),
            PkgName::from("claude"),
            InstallState::Installed,
            None,
            Some(Version::from("1.5.0-1")),
        )];
        let table = search_table(&rows, &PacmanIndex::default(), &metrics);
        assert!(
            table.lines()[0].contains("3m 20s"),
            "installed AUR row shows its build estimate: {:?}",
            table.lines()[0]
        );
    }
}
