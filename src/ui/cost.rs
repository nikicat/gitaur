//! Per-row cost cells — size + build-time — shared by the change-set table
//! ([`super::change_set`]) and the search list ([`super::search_table`]).
//!
//! Borrows the [`Paint`]/[`Width`] rendering primitives from [`super::tables`]
//! but nothing flows back the other way — `tables` never imports `cost`, so
//! there's no cycle. What lives here:
//!
//! - [`PreviewMetrics`] — the per-AUR-row overlay the shell fills in: last
//!   build duration (the only persisted cost — sizes come straight from the
//!   pacman DBs) and which rows already have artifacts on disk.
//! - [`SizeEst`]/[`size_of`] — the download/footprint size cell (exact for repo
//!   rows, estimated from the installed footprint for AUR).
//! - [`TimeEst`] — the build-time cell, plus [`built_tag`], the trailing
//!   `built` marker.

use super::grid::{Paint, Width};
use super::{human_bytes, human_duration};
use crate::names::{PkgBase, PkgName, RepoName, RepoRank};
use crate::pacman::alpm_db::PacmanIndex;
use console::style;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// Per-AUR-row cost overlay shared by the picker and the change-set preview.
///
/// Roots are keyed by [`PkgName`] (what the picker hands us) and pulled-in
/// build deps by [`PkgBase`] (what the resolver pulls): the change-set preview
/// reads both, the picker only the root maps. `stale` marks roots whose
/// recorded duration is old enough to render dimmed; `built_*` records the rows
/// whose `.pkg.tar.*` already sit in the build worktree, so a `pacman -U` would
/// reuse them instead of rebuilding.
#[derive(Debug, Default)]
pub struct PreviewMetrics {
    /// AUR root row → last successful build duration (seconds).
    pub root_build_secs: HashMap<PkgName, u64>,
    /// AUR build-dep pkgbase → last successful build duration (seconds).
    pub dep_build_secs: HashMap<PkgBase, u64>,
    /// AUR roots whose recorded `build_secs` is older than the staleness
    /// threshold — the cell is dimmed to signal the estimate is shakier than
    /// the number alone suggests.
    pub stale: HashSet<PkgName>,
    /// AUR root rows whose artifacts already sit in the build worktree.
    pub built_roots: HashSet<PkgName>,
    /// AUR build-dep pkgbases whose artifacts already sit in the worktree.
    pub built_deps: HashSet<PkgBase>,
}

impl PreviewMetrics {
    /// Empty overlay — used by tests, the single-shot `-Syu` picker (which has
    /// no loop session), and the upgrade loop when the metrics store fails to
    /// open (every AUR row then renders `?` for time and no `built` tag).
    pub fn empty() -> Self {
        Self::default()
    }
}

/// A change-set / picker row's build-time figure.
///
/// AUR roots and AUR build deps with a recorded prior duration become
/// [`Self::Estimate`] (`Xm Ys`). AUR rows the store has never seen are
/// [`Self::Unknown`] (`?`). Repo rows are [`Self::None`] — they don't build at
/// all, so the cell renders empty rather than `?` (which would imply a missing
/// measurement).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TimeEst {
    Estimate(Duration),
    Unknown,
    None,
}

impl TimeEst {
    /// The duration this row contributes to a batch total (zero when unknown or
    /// not applicable).
    pub(super) const fn contribution(self) -> Duration {
        match self {
            Self::Estimate(d) => d,
            Self::Unknown | Self::None => Duration::ZERO,
        }
    }

    /// Whether this row participates in the build-time total at all. Used to
    /// suppress the trailing `Xm Ys build` term on pure-repo batches.
    pub(super) const fn applicable(self) -> bool {
        !matches!(self, Self::None)
    }

    /// Whether this row contributes a real measurement to the total (vs. an
    /// `Unknown` that only under-counts, or a `None` that doesn't build). A
    /// total with no measured row at all collapses to [`Self::Unknown`] and
    /// renders `?` rather than a bogus `0s` — see `batch_time_total`.
    pub(super) const fn measured(self) -> bool {
        matches!(self, Self::Estimate(_))
    }

    /// Plain canonical cell text — what column widths are measured from.
    /// [`Self::None`] returns empty so a padded column collapses neatly.
    pub(super) fn render(self) -> String {
        match self {
            Self::Estimate(d) => human_duration(d),
            Self::Unknown => "?".to_owned(),
            Self::None => String::new(),
        }
    }

    /// Whether the rendered cell should be passed through [`super::dim`]: only
    /// when the user can see styling (`paint` is colored), only when the figure
    /// is [`Fade::Faded`] (stale or already built), and only on a real
    /// `Estimate` — dimming a `?` Unknown would look like a render glitch, and
    /// there's nothing to dim on `None`. Pulled out so the decision is testable
    /// without depending on `console`'s global TTY gate.
    pub(super) const fn should_dim(self, paint: Paint, fade: Fade) -> bool {
        paint.colored() && matches!(fade, Fade::Faded) && matches!(self, Self::Estimate(_))
    }
}

/// A package row's size figure — the size half of a cost cell.
///
/// Repo rows are [`Self::Exact`] (the bytes pacman will download); AUR rows are
/// an [`Self::Estimate`] from the installed version's on-disk size (rendered as
/// the bare figure — the number is the information, no marker); a row that was
/// never installed is [`Self::Unknown`] (`?`). Shared by the change-set preview
/// and the search list so the size cell (and the stale-db / zero-size bugs it's
/// had) is fixed in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SizeEst {
    Exact(u64),
    Estimate(u64),
    Unknown,
}

impl SizeEst {
    /// Bytes this row contributes to a batch total (0 when unknown).
    pub(super) const fn bytes(self) -> u64 {
        match self {
            Self::Exact(n) | Self::Estimate(n) => n,
            Self::Unknown => 0,
        }
    }

    /// The cell text: the byte figure for exact or estimate alike (an estimate
    /// carries no marker — the number is the information), `?` when unknown.
    pub(super) fn render(self) -> String {
        match self {
            Self::Exact(n) | Self::Estimate(n) => human_bytes(n),
            Self::Unknown => "?".to_owned(),
        }
    }
}

/// Size of a package row: AUR rows estimate from the installed footprint, repo
/// rows take the exact download size. Either lookup can miss → [`SizeEst::Unknown`].
/// Shared by the change-set roots and the search list.
pub(super) fn size_of(repo: &RepoName, name: &PkgName, pac: &PacmanIndex) -> SizeEst {
    if repo.rank() == RepoRank::Aur {
        pac.installed_size(name)
            .map_or(SizeEst::Unknown, SizeEst::Estimate)
    } else {
        pac.sync_download_size(name)
            .map_or(SizeEst::Unknown, SizeEst::Exact)
    }
}

/// Size of a pulled-in repo dependency: the exact bytes `pacman -S` will fetch.
pub(super) fn size_of_repo_dep(name: &PkgName, pac: &PacmanIndex) -> SizeEst {
    pac.sync_download_size(name)
        .map_or(SizeEst::Unknown, SizeEst::Exact)
}

/// Whether a build-time cell is visually de-emphasized — its recorded duration
/// is stale, or the artifact is already built so the rebuild cost is moot. A
/// named two-state rather than a bare `bool` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Fade {
    /// Render at full emphasis.
    Normal,
    /// Dim the cell.
    Faded,
}

impl From<bool> for Fade {
    fn from(faded: bool) -> Self {
        if faded { Self::Faded } else { Self::Normal }
    }
}

/// One AUR row's resolved cost state for rendering: its build-time figure plus
/// the two display flags that modulate it. Bundled into a named type so the
/// column renderers take one `RowCost` instead of a run of look-alike bools.
///
/// `stale` dims the cell (the measurement is old enough to distrust); `built`
/// means the artifact is already on disk, so the rebuild cost is moot — the
/// cell is dimmed and an `Unknown` collapses to empty (the [`built_suffix`] tag
/// carries the signal instead of a misleading `?`).
#[derive(Debug, Clone, Copy)]
pub(super) struct RowCost {
    pub(super) time: TimeEst,
    pub(super) stale: bool,
    pub(super) built: bool,
}

impl RowCost {
    /// A repo row: it never builds, so no figure and no flags.
    pub(super) const fn none() -> Self {
        Self {
            time: TimeEst::None,
            stale: false,
            built: false,
        }
    }

    /// An AUR row whose state comes straight from the overlay flags.
    pub(super) const fn aur(time: TimeEst, stale: bool, built: bool) -> Self {
        Self { time, stale, built }
    }

    /// The cell text as it renders for this row. The `to_string` round-trip
    /// respects `console`'s color gate, so piped output stays plain.
    fn cell(self, paint: Paint) -> String {
        if self.built && matches!(self.time, TimeEst::Unknown) {
            return String::new();
        }
        let s = self.time.render();
        if self
            .time
            .should_dim(paint, Fade::from(self.stale || self.built))
        {
            super::dim(s).to_string()
        } else {
            s
        }
    }

    /// Visible width of [`Self::cell`] — measured from the plain form so ANSI
    /// escapes in a dimmed cell don't skew column padding. Callers max this
    /// across rows to size the build-time column.
    pub(super) fn visible_width(self) -> Width {
        Width::of(&self.cell(Paint::Plain))
    }
}

/// Resolve the [`RowCost`] for one transaction root from the overlay, keyed by
/// its repo + pkgname (so a fresh install with no `PkgUpgrade` resolves the same
/// way an upgrade row does). Non-AUR rows never build → [`RowCost::none`]; an
/// AUR row takes its recorded duration (`Unknown` when the store has never seen
/// it) plus the stale / already-built flags. Pulled-in AUR *deps* are resolved
/// separately (by pkgbase) in the preview — see `change_set::cost_of_aur_dep`.
pub(super) fn cost_of(repo: &RepoName, name: &PkgName, metrics: &PreviewMetrics) -> RowCost {
    if repo.rank() != RepoRank::Aur {
        return RowCost::none();
    }
    let time = metrics
        .root_build_secs
        .get(name)
        .copied()
        .map_or(TimeEst::Unknown, |s| {
            TimeEst::Estimate(Duration::from_secs(s))
        });
    RowCost::aur(
        time,
        metrics.stale.contains(name),
        metrics.built_roots.contains(name),
    )
}

/// The trailing `built` tag for an already-built AUR row — green when colored,
/// plain otherwise. Rendered unaligned at the end of the row, like the session
/// badges, so it never perturbs column math.
fn built_tag(paint: Paint) -> String {
    if paint.colored() {
        style("built").green().to_string()
    } else {
        "built".to_owned()
    }
}

/// A right-justified build-time column padded to `width` visible columns. The
/// pad is measured from the plain cell so a dimmed estimate's ANSI escapes
/// don't skew it. AUR rows fill the column; repo rows ([`RowCost::none`])
/// collapse to blanks that keep it aligned.
pub(super) fn time_col(cost: RowCost, width: Width, paint: Paint) -> String {
    format!("{}{}", width.gap(cost.visible_width()), cost.cell(paint))
}

/// The trailing `  built` tag (with its leading gap) for an already-built row,
/// or empty otherwise. Unaligned — appended after the last aligned column, like
/// the session badges, so it never perturbs column math.
pub(super) fn built_suffix(cost: RowCost, paint: Paint) -> String {
    if cost.built {
        format!("  {}", built_tag(paint))
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A build duration from a plain seconds count — the store's native unit.
    fn dur(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    /// `TimeEst` renders the three meaningful cells; `None` collapses to empty
    /// so the column padding does the right thing for repo rows.
    #[test]
    fn time_est_renders_each_variant() {
        let est = |secs| TimeEst::Estimate(dur(secs));
        assert_eq!(est(45).render(), "45s");
        assert_eq!(est(125).render(), "2m 5s");
        assert_eq!(est(3_725).render(), "1h 2m");
        assert_eq!(TimeEst::Unknown.render(), "?");
        assert_eq!(TimeEst::None.render(), "");
    }

    /// `should_dim` is the decision behind the dim affordance. Only the exact
    /// combination `(Colored, Faded, Estimate)` qualifies; the other axes all
    /// suppress it.
    #[test]
    fn time_est_should_dim_truth_table() {
        let est = TimeEst::Estimate(dur(60));
        assert!(est.should_dim(Paint::Colored, Fade::Faded));
        assert!(
            !est.should_dim(Paint::Plain, Fade::Faded),
            "plain must skip dim"
        );
        assert!(
            !est.should_dim(Paint::Colored, Fade::Normal),
            "non-faded must skip dim"
        );
        assert!(
            !TimeEst::Unknown.should_dim(Paint::Colored, Fade::Faded),
            "Unknown must never dim — `?` dimmed looks like a render glitch",
        );
        assert!(
            !TimeEst::None.should_dim(Paint::Colored, Fade::Faded),
            "None has no cell to dim"
        );
    }

    /// A built `Unknown` row renders an empty time cell (the `built` tag carries
    /// the signal), while a built `Estimate` keeps its number; `visible_width`
    /// tracks the cell actually rendered, not the canonical `render()`.
    #[test]
    fn built_unknown_cell_is_empty() {
        let built_unknown = RowCost::aur(TimeEst::Unknown, false, true);
        assert_eq!(built_unknown.cell(Paint::Plain), "");
        assert_eq!(built_unknown.visible_width().cells(), 0);
        // Not built: the Unknown row still shows `?`.
        let unknown = RowCost::aur(TimeEst::Unknown, false, false);
        assert_eq!(unknown.cell(Paint::Plain), "?");
        assert_eq!(unknown.visible_width().cells(), 1);
        // A built estimate keeps its plain text (dimming only adds ANSI, which
        // the plain-paint path skips).
        assert_eq!(
            RowCost::aur(TimeEst::Estimate(dur(60)), false, true).cell(Paint::Plain),
            "1m 0s"
        );
    }

    /// `built_suffix` is the unaligned trailing tag: present iff the row is
    /// built, with its leading gap; the plain form is exactly `  built`.
    #[test]
    fn built_suffix_only_when_built() {
        assert_eq!(
            built_suffix(RowCost::aur(TimeEst::Unknown, false, true), Paint::Plain),
            "  built"
        );
        assert_eq!(
            built_suffix(RowCost::aur(TimeEst::Unknown, false, false), Paint::Plain),
            ""
        );
        assert_eq!(built_suffix(RowCost::none(), Paint::Plain), "");
    }

    /// Each `SizeEst` variant renders its expected cell.
    #[test]
    fn size_est_renders_each_variant() {
        assert_eq!(SizeEst::Exact(1024).render(), "1.00 KiB");
        assert_eq!(SizeEst::Estimate(1024).render(), "1.00 KiB");
        assert_eq!(SizeEst::Unknown.render(), "?");
    }

    /// A root's size source is chosen by repo: AUR rows estimate from localdb
    /// `isize`, repo rows take the exact syncdb download size, and a miss in
    /// either map falls back to unknown.
    #[test]
    fn size_of_picks_source_by_repo() {
        let mut pac = PacmanIndex::default();
        pac.installed_size
            .insert("paru-bin".into(), 9 * 1024 * 1024);
        pac.sync_download_size
            .insert("glibc".into(), 12 * 1024 * 1024);

        assert_eq!(
            size_of(&"aur".into(), &"paru-bin".into(), &pac),
            SizeEst::Estimate(9 * 1024 * 1024)
        );
        assert_eq!(
            size_of(&"core".into(), &"glibc".into(), &pac),
            SizeEst::Exact(12 * 1024 * 1024)
        );
        // AUR row with no localdb size (manually built / never installed).
        assert_eq!(
            size_of(&"aur".into(), &"ghost".into(), &pac),
            SizeEst::Unknown
        );
    }

    /// Regression guard for the stale-db size bug: a repo row whose pkgname is
    /// present with a `download_size` of 0 (libalpm's answer for an already-cached
    /// archive) is `Exact(0)` → renders `0 B`, distinct from a *missing* pkgname,
    /// which is `Unknown` → `?`.
    #[test]
    fn repo_zero_size_is_exact_not_missing() {
        let mut pac = PacmanIndex::default();
        pac.sync_download_size.insert("cached".into(), 0);
        let cached = size_of(&"core".into(), &"cached".into(), &pac);
        assert_eq!(cached, SizeEst::Exact(0));
        assert_eq!(cached.render(), "0 B");
        let missing = size_of(&"core".into(), &"absent".into(), &pac);
        assert_eq!(missing, SizeEst::Unknown);
        assert_eq!(missing.render(), "?");
    }
}
