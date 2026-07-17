//! The unified staged-transaction table for the interactive shell.
//!
//! `show` resolves the cart into a change set and renders it through
//! [`transaction_table`]: the numbered, approval-tagged root rows (repo + AUR,
//! install + upgrade) over the same sort/verdiff/size/build-time machinery the
//! old upgrade-loop preview used, plus the pulled-in dependency rows, the
//! "will remove" rows, and a batch total. `apply` no longer redraws it — it
//! gates on the one-line [`cost_summary`] instead (`docs/plans/shell-ui.md`,
//! phase 5a). The renderers *return* their lines so the shell owns the output
//! stream and the layout is unit-testable.
//!
//! Two cost figures live here:
//! - **Size** — exact `download_size` from the syncdb for repo rows; the `isize`
//!   from localdb (rendered as the bare figure) for AUR rows; `?` for
//!   never-installed pull-ins.
//! - **Build time** — `Xm Ys` from the cross-session `MetricsStore` for AUR
//!   rows that have ever been built before; `?` for first-time builds the
//!   store can't predict; dimmed when the recorded duration is old enough that
//!   it's a shaky predictor. A *summed* total that under-counts (an unknown row
//!   contributed 0) is a lower bound, prefixed `>`.

use super::cells::{VersionColumn, repo_cell};
use super::cost::{
    PreviewMetrics, RowCost, SizeEst, TimeEst, built_suffix, cost_of, size_of, size_of_repo_dep,
};
use super::grid::{Cell, Col, Grid, GridRow, Paint, Table, Width};
use super::{dim, human_age, human_bytes, human_duration};
use crate::names::{PkgBase, PkgName, RepoName};
use crate::pacman::alpm_db::PacmanIndex;
use crate::version::Version;
use console::style;
use std::fmt::Write as _;
use std::time::Duration;

/// The presentation half of the shell's `cart::Approval`.
///
/// Kept here so the renderer owns the approval label + color without `ui`
/// depending on `cli::shell`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalCell {
    Approved,
    NeedsReview,
}

impl ApprovalCell {
    /// Plain column text — also what the column width is measured from.
    const fn label(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::NeedsReview => "review",
        }
    }

    /// The aligned cell: green when approved, yellow when pending; plain when
    /// color is off.
    fn cell(self, paint: Paint) -> Cell {
        Cell::paint(self.label(), paint, |s| match self {
            Self::Approved => style(s).green().to_string(),
            Self::NeedsReview => style(s).yellow().to_string(),
        })
    }
}

/// One numbered root row of the staged transaction.
///
/// Built by the shell from a `cart::CartItem` plus index/db lookups: an upgrade
/// row carries both versions, a fresh install leaves `old_ver` `None` (and the
/// renderer drops the arrow). `age` is the AUR pkgbase's "last modified" age
/// (the shell reads the wall clock); `None` for repo rows or when the index has
/// no commit time.
#[derive(Debug, Clone)]
pub struct TxnRoot {
    pub repo: RepoName,
    pub approval: ApprovalCell,
    pub name: PkgName,
    /// `None` for a fresh install; `Some` for an upgrade.
    pub old_ver: Option<Version>,
    /// The version being installed; `None` only when we couldn't resolve one
    /// (the renderer then leaves the version cell blank but aligned).
    pub new_ver: Option<Version>,
    /// AUR pkgbase "last modified" age, for the trailing `(Xd ago)` cell.
    pub age: Option<Duration>,
}

/// One staged transaction's full change set, bundled for rendering.
///
/// The user-named roots, the pulled-in dependencies, the staged removals,
/// and the two figure sources (sizes from `pac`, build times from
/// `metrics`) — the value `transaction_table` / `cost_summary` used to take
/// as six parallel parameters.
pub struct ChangeSet<'a> {
    pub roots: &'a [TxnRoot],
    pub repo_deps: &'a [PkgName],
    pub aur_deps: &'a [PkgBase],
    pub removals: &'a [PkgName],
    pub pac: &'a PacmanIndex,
    pub metrics: &'a PreviewMetrics,
}

impl ChangeSet<'_> {
    /// Render the unified transaction table as display lines.
    ///
    /// No trailing newline, and no top header — the shell's `show` prints its own
    /// header + approval summary around these. `roots` are rendered in the order
    /// given (the cart holds them sorted, so the row number *is* the cart index);
    /// `repo_deps` / `aur_deps` are the pulled-in dependencies the resolver added;
    /// `removals` are the staged uninstalls; `pac` backs the size figures and
    /// `metrics` the build-time ones. `paint` is passed in (callers use
    /// [`Paint::detect`]) rather than re-read from the environment, so tests can
    /// pin the plain rendering.
    pub fn table(&self, paint: Paint) -> Table {
        let fig = self.figures();
        let versions = VersionColumn::measure(
            self.roots
                .iter()
                .map(|r| (r.old_ver.as_ref(), r.new_ver.as_ref())),
        );

        // The size and build-time columns span the root rows *and* the dep
        // block: measure each over the union once and hand both grids the
        // result as a width floor, so the figures line up across sections.
        let size_w = Width::widest(
            fig.root_sizes
                .iter()
                .chain(&fig.repo_dep_sizes)
                .chain(&fig.aur_dep_sizes)
                .map(|s| Width::of(&s.render())),
        );
        let time_w = Width::widest(
            fig.root_costs
                .iter()
                .chain(&fig.aur_dep_costs)
                .map(|c| c.cell(paint).width()),
        );

        let mut roots = Grid::new(vec![
            Col::right(),             // №
            Col::left(),              // repo
            Col::left(),              // approval
            Col::left(),              // name
            Col::left(),              // version block
            Col::right().min(size_w), // size — shared with the dep block
            Col::right().min(time_w), // build time — shared with the dep block
        ]);
        for (i, ((root, size), cost)) in self
            .roots
            .iter()
            .zip(&fig.root_sizes)
            .zip(&fig.root_costs)
            .enumerate()
        {
            roots.push(
                GridRow::new(vec![
                    Cell::plain((i + 1).to_string()),
                    repo_cell(&root.repo, paint),
                    root.approval.cell(paint),
                    Cell::plain(root.name.as_str()),
                    versions.cell(root.old_ver.as_ref(), root.new_ver.as_ref(), paint),
                    Cell::plain(size.render()),
                    cost.cell(paint),
                ])
                .tail(format!(
                    "{}{}",
                    built_suffix(*cost, paint),
                    age_cell(root.age, paint)
                )),
            );
        }

        let mut out = roots.render();
        out.append(dep_lines(
            self.repo_deps,
            self.aur_deps,
            &fig,
            size_w,
            time_w,
            paint,
        ));
        out.append(removal_lines(self.removals, paint));

        if let Some(total) = total_line(&fig) {
            out.push(total);
        }
        out
    }

    /// The one-line cost summary `apply` prints before running — a receipt,
    /// not a gate (the explicit `apply` command is the consent).
    ///
    /// `show` is where the user looks; `apply` no longer redraws the table. E.g.
    /// `3 install, +2 deps, 1 remove · 3.07 GiB · 22m build`. The deps / remove /
    /// build terms are omitted when their count is zero. A total that under-counts
    /// because some row's figure is unknown is a lower bound, prefixed `>`.
    pub fn summary(&self) -> String {
        let fig = self.figures();
        let size = fig.size_total();
        let time = fig.time_total();

        let mut parts = vec![format!("{} install", self.roots.len())];
        let deps = self.repo_deps.len() + self.aur_deps.len();
        if deps > 0 {
            parts.push(format!("+{deps} dep{}", if deps == 1 { "" } else { "s" }));
        }
        if !self.removals.is_empty() {
            parts.push(format!("{} remove", self.removals.len()));
        }
        let mut line = parts.join(", ");
        write!(line, " · {}", size.render()).ok();
        if let Some(build) = build_term(time) {
            write!(line, " · {build}").ok();
        }
        line
    }

    /// Compute the per-row size + build-time figures once, shared by
    /// [`Self::table`] (per-cell + widths) and [`Self::summary`] (totals).
    fn figures(&self) -> Figures {
        Figures {
            root_sizes: self
                .roots
                .iter()
                .map(|r| size_of(&r.repo, &r.name, self.pac))
                .collect(),
            root_costs: self
                .roots
                .iter()
                .map(|r| cost_of(&r.repo, &r.name, self.metrics))
                .collect(),
            repo_dep_sizes: self
                .repo_deps
                .iter()
                .map(|n| size_of_repo_dep(n, self.pac))
                .collect(),
            // Pulled-in AUR deps are unsatisfied builds — not yet installed — so
            // their footprint is unknown (`?`).
            aur_dep_sizes: vec![SizeEst::Unknown; self.aur_deps.len()],
            aur_dep_costs: self
                .aur_deps
                .iter()
                .map(|pb| cost_of_aur_dep(pb, self.metrics))
                .collect(),
        }
    }
}

/// The indented "pulls in:" block: the repo deps (`(install)`) then the AUR
/// build deps (`(build)`), each with its size and — for AUR rows — build-time
/// cell. Empty (no marker) when nothing is pulled in. `size_w` / `time_w` are
/// the columns shared with the root rows so the figures line up beneath them.
fn dep_lines(
    repo_deps: &[PkgName],
    aur_deps: &[PkgBase],
    fig: &Figures,
    size_w: Width,
    time_w: Width,
    paint: Paint,
) -> Table {
    if repo_deps.is_empty() && aur_deps.is_empty() {
        return Table::new();
    }
    let mut grid = Grid::new(vec![
        Col::left(), // dep name
        // "(install)" is the widest tag — a floor so the size column lines up
        // across install and build rows even when only "(build)" rows exist.
        Col::left().min(Width::of("(install)")),
        Col::right().min(size_w), // size — shared with the root rows
        Col::right().min(time_w), // build time — shared with the root rows
    ])
    .indent("        ");
    for (name, size) in repo_deps.iter().zip(&fig.repo_dep_sizes) {
        grid.push(GridRow::new(vec![
            Cell::plain(name.as_str()),
            Cell::plain("(install)"),
            Cell::plain(size.render()),
            Cell::plain(""),
        ]));
    }
    for ((name, size), cost) in aur_deps
        .iter()
        .zip(&fig.aur_dep_sizes)
        .zip(&fig.aur_dep_costs)
    {
        grid.push(
            GridRow::new(vec![
                Cell::plain(name.as_str()),
                Cell::plain("(build)"),
                Cell::plain(size.render()),
                cost.cell(paint),
            ])
            .tail(built_suffix(*cost, paint)),
        );
    }
    let mut out = Table::new();
    out.push(marker("pulls in:", paint));
    out.append(grid.render());
    out
}

/// The indented "will remove:" block, red when colored; empty (no marker) when
/// nothing is staged for removal.
fn removal_lines(removals: &[PkgName], paint: Paint) -> Table {
    if removals.is_empty() {
        return Table::new();
    }
    let mut out = Table::new();
    out.push(marker("will remove:", paint));
    for name in removals {
        let shown = if paint.colored() {
            style(name.as_str()).red().to_string()
        } else {
            name.as_str().to_owned()
        };
        out.push(format!("        {shown}"));
    }
    out
}

/// The trailing ` build` term for the one-line [`ChangeSet::summary`], or
/// `None` for a pure-repo batch that carries no build-time term. An
/// all-unknown total renders `? build` — never a bogus `0s build`, the
/// never-built case; a total that under-counts because an unknown row is in
/// the mix is a lower bound (`>22m build`). The table's [`total_line`] renders
/// its own 🔨 term and drops the all-unknown case entirely.
fn build_term(time: TimeTotal) -> Option<String> {
    match time {
        TimeTotal::None => None,
        TimeTotal::Unknown => Some("? build".to_owned()),
        TimeTotal::Measured { total, bound } => {
            Some(format!("{}{} build", bound.marker(), human_duration(total)))
        }
    }
}

/// Whether a summed figure is exact, or a lower bound because a row with an
/// unknown figure contributed 0 to the sum — so the true total is *greater
/// than* what's shown. Renders the leading `>` a lower-bound total carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bound {
    Exact,
    Lower,
}

impl Bound {
    /// The `>` prefix a lower-bound total carries, empty for an exact one.
    const fn marker(self) -> &'static str {
        match self {
            Self::Exact => "",
            Self::Lower => ">",
        }
    }
}

impl From<bool> for Bound {
    fn from(lower: bool) -> Self {
        if lower { Self::Lower } else { Self::Exact }
    }
}

/// The per-row size + build-time figures for a change set, computed once and
/// shared by [`ChangeSet::table`] (per-cell + widths) and [`ChangeSet::summary`]
/// (totals only).
struct Figures {
    root_sizes: Vec<SizeEst>,
    root_costs: Vec<RowCost>,
    repo_dep_sizes: Vec<SizeEst>,
    aur_dep_sizes: Vec<SizeEst>,
    aur_dep_costs: Vec<RowCost>,
}

impl Figures {
    /// Total size across roots + repo deps + AUR deps.
    fn size_total(&self) -> SizeTotal {
        batch_size_total(
            self.root_sizes
                .iter()
                .chain(&self.repo_dep_sizes)
                .chain(&self.aur_dep_sizes)
                .copied(),
        )
    }

    /// Total build time across the AUR roots + AUR deps.
    fn time_total(&self) -> TimeTotal {
        batch_time_total(
            self.root_costs
                .iter()
                .chain(&self.aur_dep_costs)
                .map(|c| c.time),
        )
    }
}

/// The batch `total` line for the table, or `None` when there is nothing to
/// total: the size figure behind 📥, the build-time figure behind 🔨. Either
/// term joins only when something was actually *measured* — an all-unknown
/// figure shows nothing rather than a noisy `📥 ?` / `🔨 ?` (the per-row `?`
/// cells already carry that; the one-line [`ChangeSet::summary`] keeps its
/// explicit `?` terms). A batch with neither figure drops the line entirely.
fn total_line(fig: &Figures) -> Option<String> {
    let mut terms = Vec::new();
    if let SizeTotal::Known { total, bound } = fig.size_total() {
        terms.push(format!("📥 {}{}", bound.marker(), total.render()));
    }
    if let TimeTotal::Measured { total, bound } = fig.time_total() {
        terms.push(format!("🔨 {}{}", bound.marker(), human_duration(total)));
    }
    (!terms.is_empty()).then(|| format!("-> total  {}", terms.join("   ")))
}

/// A section marker line (`-> pulls in:` / `-> will remove:`), dimmed when
/// colored.
fn marker(text: &str, paint: Paint) -> String {
    let body = format!("-> {text}");
    if paint.colored() {
        dim(&body).to_string()
    } else {
        body
    }
}

/// The trailing `  (Xd ago)` age cell for an AUR row, dimmed when colored; empty
/// when there's no age.
fn age_cell(age: Option<Duration>, paint: Paint) -> String {
    let Some(age) = age else {
        return String::new();
    };
    let label = format!("({} ago)", human_age(age));
    if paint.colored() {
        format!("  {}", dim(&label))
    } else {
        format!("  {label}")
    }
}

/// A byte count. A newtype (not a bare `u64`) so a size can't be mixed up with a
/// package count or a duration; renders through [`human_bytes`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Bytes(u64);

impl Bytes {
    const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    fn render(self) -> String {
        human_bytes(self.0)
    }
}

/// The summed download/footprint size of a change set. A sum can under-count —
/// an `Unknown` row (a never-installed AUR pkg) contributes 0 but really adds
/// more — so a total with any unknown in the mix is a [`Bound::Lower`] bound,
/// rendered `>`. An all-unknown total has no figure at all and renders `?`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeTotal {
    /// Every contributing row was `Unknown` — no figure, renders `?`.
    Unknown,
    /// At least one row carried a size; `bound` is `Lower` if an unknown row is
    /// also in the mix.
    Known { total: Bytes, bound: Bound },
}

impl SizeTotal {
    fn render(self) -> String {
        match self {
            Self::Unknown => "?".to_owned(),
            Self::Known { total, bound } => format!("{}{}", bound.marker(), total.render()),
        }
    }
}

/// Sum a change set's size figures. `Unknown` when every row is unknown;
/// otherwise `Known` with the summed bytes, flagged a lower bound the moment an
/// unknown row (contributing 0) is in the mix.
fn batch_size_total(sizes: impl IntoIterator<Item = SizeEst>) -> SizeTotal {
    let mut total = Bytes::default();
    let mut known = false;
    let mut any_unknown = false;
    for s in sizes {
        total = total.saturating_add(Bytes(s.bytes()));
        known |= !matches!(s, SizeEst::Unknown);
        any_unknown |= matches!(s, SizeEst::Unknown);
    }
    if known {
        SizeTotal::Known {
            total,
            bound: any_unknown.into(),
        }
    } else {
        SizeTotal::Unknown
    }
}

/// Cost cell for one pulled-in AUR build dep — by definition an AUR build, so
/// the figure is Estimate or Unknown (never None). Dep cells aren't dimmed for
/// staleness today, but a built dep still shows the `built` tag.
fn cost_of_aur_dep(pb: &PkgBase, metrics: &PreviewMetrics) -> RowCost {
    let time = metrics
        .dep_build_secs
        .get(pb)
        .copied()
        .map_or(TimeEst::Unknown, |s| {
            TimeEst::Estimate(Duration::from_secs(s))
        });
    RowCost::aur(time, false, metrics.built_deps.contains(pb))
}

/// The summed build time of a change set. Unlike a per-row [`TimeEst`], a *sum*
/// can be a lower bound: some rows measured, others `Unknown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeTotal {
    /// Pure-repo batch — no build-time term at all.
    None,
    /// Builds, but nothing was ever measured — renders `?`.
    Unknown,
    /// At least one measured row; `bound` is `Lower` when an `Unknown` row is
    /// also in the mix, so the sum under-counts (`>22m` vs `22m`).
    Measured { total: Duration, bound: Bound },
}

/// Sum a change set's build-time figures. `Measured` once any row carries a real
/// duration (a lower bound if an unknown row is mixed in); `Unknown` when the
/// batch builds but nothing was measured (renders `?`, never a bogus `0s`);
/// `None` for a pure-repo batch (no build-time term).
fn batch_time_total(times: impl IntoIterator<Item = TimeEst>) -> TimeTotal {
    let mut total = Duration::ZERO;
    let mut measured = false;
    let mut applicable = false;
    let mut any_unknown = false;
    for t in times {
        total = total.saturating_add(t.contribution());
        measured |= t.measured();
        applicable |= t.applicable();
        any_unknown |= matches!(t, TimeEst::Unknown);
    }
    match (measured, applicable) {
        (true, _) => TimeTotal::Measured {
            total,
            bound: any_unknown.into(),
        },
        (false, true) => TimeTotal::Unknown,
        (false, false) => TimeTotal::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture: assemble a [`ChangeSet`] from a test's pieces.
    fn cs<'a>(
        roots: &'a [TxnRoot],
        repo_deps: &'a [PkgName],
        aur_deps: &'a [PkgBase],
        removals: &'a [PkgName],
        pac: &'a PacmanIndex,
        metrics: &'a PreviewMetrics,
    ) -> ChangeSet<'a> {
        ChangeSet {
            roots,
            repo_deps,
            aur_deps,
            removals,
            pac,
            metrics,
        }
    }

    use crate::{assert_contains, assert_not_contains, assert_regex};
    use console::strip_ansi_codes;

    fn root(repo: &str, name: &str, old: Option<&str>, new: Option<&str>) -> TxnRoot {
        TxnRoot {
            repo: RepoName::from(repo),
            approval: ApprovalCell::Approved,
            name: PkgName::from(name),
            old_ver: old.map(Version::from),
            new_ver: new.map(Version::from),
            age: None,
        }
    }

    /// A build duration from a plain seconds count — the store's native unit.
    fn dur(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    /// Byte sizes for fixtures, readable at the call site (Rust has no `**`
    /// operator, so `n * 1024 * 1024` chains were the alternative).
    const fn mib(n: u64) -> u64 {
        n << 20
    }
    const fn gib(n: u64) -> u64 {
        n << 30
    }

    /// Each `SizeEst` variant renders its expected cell (no `~` prefix — an
    /// estimate reads the same as an exact figure; unknown is a bare `?`).
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
        pac.installed_size.insert("paru-bin".into(), mib(9));
        pac.sync_download_size.insert("glibc".into(), mib(12));

        assert_eq!(
            size_of(&"aur".into(), &"paru-bin".into(), &pac),
            SizeEst::Estimate(mib(9))
        );
        assert_eq!(
            size_of(&"core".into(), &"glibc".into(), &pac),
            SizeEst::Exact(mib(12))
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

    /// The size total sums bytes; an estimate reads as an exact total (no `~`),
    /// but an `Unknown` row makes the total a lower bound (`>`), and an
    /// all-unknown total has no figure (`?`).
    #[test]
    fn batch_size_total_sums_and_marks_lower_bound() {
        assert_eq!(
            batch_size_total([SizeEst::Exact(100), SizeEst::Exact(200)]),
            SizeTotal::Known {
                total: Bytes(300),
                bound: Bound::Exact,
            },
            "all-exact total is an exact figure"
        );
        assert_eq!(
            batch_size_total([SizeEst::Exact(100), SizeEst::Estimate(50)]),
            SizeTotal::Known {
                total: Bytes(150),
                bound: Bound::Exact,
            },
            "an estimate still reads as an exact total — no ~, no >"
        );
        assert_eq!(
            batch_size_total([SizeEst::Exact(100), SizeEst::Unknown]),
            SizeTotal::Known {
                total: Bytes(100),
                bound: Bound::Lower,
            },
            "an unknown row (0 bytes) makes the total a lower bound"
        );
        assert_eq!(
            batch_size_total([SizeEst::Unknown, SizeEst::Unknown]),
            SizeTotal::Unknown,
            "every row unknown → no figure at all"
        );
    }

    /// Source selection for the root cost cell: repo rows are not-applicable
    /// (`None`, never built); AUR rows resolve through `root_build_secs`, an
    /// unrecorded AUR row is `Unknown`, and the stale/built flags come from the
    /// overlay sets.
    #[test]
    fn cost_of_picks_by_repo_and_flags() {
        let mut metrics = PreviewMetrics::empty();
        metrics
            .root_build_secs
            .insert(PkgName::from("paru-bin"), 90);
        metrics.built_roots.insert(PkgName::from("paru-bin"));
        metrics.stale.insert(PkgName::from("first-time"));

        let repo = cost_of(&"core".into(), &"glibc".into(), &metrics);
        assert_eq!(repo.time, TimeEst::None);
        assert!(!repo.built);

        let recorded = cost_of(&"aur".into(), &"paru-bin".into(), &metrics);
        assert_eq!(recorded.time, TimeEst::Estimate(dur(90)));
        assert!(recorded.built);
        assert!(!recorded.stale);

        let first = cost_of(&"aur".into(), &"first-time".into(), &metrics);
        assert_eq!(first.time, TimeEst::Unknown);
        assert!(first.stale);
        assert!(!first.built);
    }

    /// Pulled-in AUR build deps are always AUR builds — Estimate or Unknown, never
    /// None; the built flag tracks `built_deps`.
    #[test]
    fn cost_of_aur_dep_resolves_or_unknown() {
        let mut metrics = PreviewMetrics::empty();
        metrics
            .dep_build_secs
            .insert(PkgBase::from("nvidia-utils"), 600);
        metrics.built_deps.insert(PkgBase::from("nvidia-utils"));

        let recorded = cost_of_aur_dep(&PkgBase::from("nvidia-utils"), &metrics);
        assert_eq!(recorded.time, TimeEst::Estimate(dur(600)));
        assert!(recorded.built);

        let unknown = cost_of_aur_dep(&PkgBase::from("never-built"), &metrics);
        assert_eq!(unknown.time, TimeEst::Unknown);
        assert!(!unknown.built);
    }

    /// The build-time total: `None` for a pure-repo batch (no term), `Unknown`
    /// when the batch builds but nothing was measured (renders `?`, never `0s`),
    /// `Measured` once any row carries a real duration — and a lower bound the
    /// moment a measured row and an `Unknown` row share the batch.
    #[test]
    fn batch_time_total_collapses_and_marks_lower_bound() {
        let est = |secs| TimeEst::Estimate(dur(secs));
        assert_eq!(
            batch_time_total([TimeEst::None, TimeEst::None]),
            TimeTotal::None,
            "pure-repo batch carries no build-time term"
        );
        assert_eq!(
            batch_time_total([TimeEst::Unknown, TimeEst::None]),
            TimeTotal::Unknown,
            "builds but nothing measured → Unknown (renders ?, not 0s)"
        );
        assert_eq!(
            batch_time_total([est(60), est(120)]),
            TimeTotal::Measured {
                total: dur(180),
                bound: Bound::Exact,
            },
            "all measured → an exact total"
        );
        assert_eq!(
            batch_time_total([est(60), est(120), TimeEst::Unknown, TimeEst::None]),
            TimeTotal::Measured {
                total: dur(180),
                bound: Bound::Lower,
            },
            "an unmeasured build makes the measured sum a lower bound"
        );
    }

    /// Regression (docs/TODO.md): a single never-built AUR package has an
    /// `Unknown` build time, so the summary must read `? build`, not the
    /// misleading `0s build` that summed an unmeasured 0.
    #[test]
    fn cost_summary_never_built_shows_unknown_not_zero() {
        let mut pac = PacmanIndex::default();
        pac.installed_size.insert("newthing".into(), mib(85));
        let roots = vec![root("aur", "newthing", None, Some("1.0-1"))];
        let s = cs(&roots, &[], &[], &[], &pac, &PreviewMetrics::empty()).summary();
        assert_contains!(s, "? build", "never-built build time is unknown");
        assert_not_contains!(s, "0s build", "must not fake a summed figure");
    }

    /// One numbered row per root, in the given order; a fresh install (no `old`)
    /// drops the arrow while an upgrade keeps `old -> new`; the deps, removals,
    /// and total lines all appear. [`Paint::Plain`] is pinned — the colored form
    /// uses a Unicode arrow and ANSI verdiff splits, so inheriting the ambient
    /// terminal's paint made this fail under an interactive `makepkg check()`
    /// while passing on tty-less CI.
    #[test]
    fn transaction_table_renders_rows_deps_and_total() {
        let mut pac = PacmanIndex::default();
        pac.sync_download_size.insert("glibc".into(), mib(12));
        pac.sync_download_size.insert("gcc13".into(), mib(50));
        pac.installed_size.insert("cuda".into(), gib(3));

        let roots = vec![
            root("core", "glibc", Some("2.40-1"), Some("2.41-1")),
            root("aur", "cuda", Some("12.6-1"), Some("12.8-1")),
            root("extra", "newpkg", None, Some("1.0-1")), // fresh install
        ];
        let repo_deps = vec![PkgName::from("gcc13")];
        let aur_deps = vec![PkgBase::from("nvidia-utils")];
        let removals = vec![PkgName::from("old-cuda")];
        let metrics = PreviewMetrics::empty();

        let table =
            cs(&roots, &repo_deps, &aur_deps, &removals, &pac, &metrics).table(Paint::Plain);
        let lines = table.lines();
        // One pattern per row pins the column order: number (as wide as the
        // row count's digit count — 1 here), repo, then name and versions.
        assert_regex!(lines[0], r"^1  core\s+.*glibc\s+2\.40-1 -> 2\.41-1");
        assert_regex!(lines[1], r"^2  aur\s+.*cuda\s+12\.6-1 -> 12\.8-1");
        // Fresh install: no arrow, just the new version after a blank gap.
        assert_regex!(lines[2], r"^3  extra\s+.*newpkg\s+1\.0-1");
        assert_not_contains!(lines[2], "->", "fresh install has no arrow");

        let joined = lines.join("\n");
        // Dep rows pair each name with its tag on the same line; removals sit
        // directly under their marker.
        assert_regex!(joined, "(?m)^-> pulls in:");
        assert_regex!(joined, r"(?m)^\s+gcc13\s+\(install\)");
        assert_regex!(joined, r"(?m)^\s+nvidia-utils\s+\(build\)");
        assert_regex!(joined, r"-> will remove:\n\s+old-cuda");
        assert_regex!(joined, r"(?m)^-> total  📥 \S");
        // The batch has AUR rows but nothing measured (empty metrics): the
        // total's build term is omitted, not rendered as an unknown figure.
        assert_not_contains!(
            lines.last().unwrap(),
            "🔨",
            "unmeasured builds leave no build term on the total"
        );
    }

    /// A pure-repo cart with no deps/removals: just numbered rows + a total with
    /// no build-time term.
    #[test]
    fn transaction_table_pure_repo_no_build_term() {
        let mut pac = PacmanIndex::default();
        pac.sync_download_size.insert("glibc".into(), 1024);
        let roots = vec![root("core", "glibc", Some("1-1"), Some("1-2"))];
        let table = cs(&roots, &[], &[], &[], &pac, &PreviewMetrics::empty()).table(Paint::Plain);
        let total = table.lines().last().unwrap();
        assert_regex!(total, r"^-> total  📥 \S");
        assert_not_contains!(total, "🔨", "pure-repo total has no build term");
    }

    /// An all-unknown size total (a fresh AUR install, nothing measured)
    /// drops the whole total line — the per-row `?` cells already carry the
    /// unknown, and a `📥 ?` total adds noise, not information. (Found by the
    /// README screencast review, docs/plans/screencasts.md.)
    #[test]
    fn transaction_table_all_unknown_drops_total_line() {
        let pac = PacmanIndex::default();
        let roots = vec![root("aur", "newpkg", None, Some("1.0-1"))];
        let table = cs(&roots, &[], &[], &[], &pac, &PreviewMetrics::empty()).table(Paint::Plain);
        assert_not_contains!(
            table.lines().last().unwrap(),
            "-> total",
            "all-unknown batch renders no total line"
        );
    }

    /// A measured AUR build joins the total as the 🔨 term — exact when every
    /// build row is measured, a `>` lower bound once an unknown build is in
    /// the mix. (The all-unknown case omits the term; covered above.)
    #[test]
    fn transaction_table_total_build_term_measured_and_bound() {
        let mut pac = PacmanIndex::default();
        pac.installed_size.insert("cuda".into(), gib(3));
        let roots = vec![root("aur", "cuda", Some("1-1"), Some("2-1"))];
        let mut metrics = PreviewMetrics::empty();
        metrics.root_build_secs.insert(PkgName::from("cuda"), 120);

        let table = cs(&roots, &[], &[], &[], &pac, &metrics).table(Paint::Plain);
        assert_regex!(
            table.lines().last().unwrap(),
            r"^-> total  📥 3\.00 GiB   🔨 2m 0s$"
        );

        // An unmeasured build dep contributes 0 → the sum is a lower bound.
        let aur_deps = vec![PkgBase::from("never-built")];
        let table = cs(&roots, &[], &aur_deps, &[], &pac, &metrics).table(Paint::Plain);
        assert_regex!(table.lines().last().unwrap(), "🔨 >2m 0s$");
    }

    /// Both paints render the same *content*: stripping ANSI codes from the
    /// colored table and reading token-by-token must reproduce the plain one,
    /// with the arrow glyph (`→` colored, `->` plain) as the single mapped
    /// token. Spacing is deliberately not compared — the arrow widths differ
    /// per paint ([`Paint::arrow`]) and column alignment has its own test
    /// below. One cart exercises every `Paint::Colored` arm: the
    /// verdiff-split upgrade row, the green fresh install, the dimmed
    /// marker/age cells, and the red removals.
    #[test]
    fn transaction_table_colored_strips_to_plain() {
        let mut pac = PacmanIndex::default();
        pac.sync_download_size.insert("glibc".into(), mib(12));
        pac.sync_download_size.insert("gcc13".into(), mib(50));
        pac.installed_size.insert("cuda".into(), gib(3));
        let mut roots = vec![
            root("core", "glibc", Some("2.40-1"), Some("2.41-1")),
            root("aur", "cuda", Some("12.6-1"), Some("12.8-1")),
            root("extra", "newpkg", None, Some("1.0-1")), // fresh install
        ];
        // `root()` leaves age unset; give the AUR row one so the dimmed
        // `(Xd ago)` cell renders too.
        roots[1].age = Some(dur(3 * 24 * 3600));
        let repo_deps = vec![PkgName::from("gcc13")];
        let aur_deps = vec![PkgBase::from("nvidia-utils")];
        let removals = vec![PkgName::from("old-cuda")];
        let metrics = PreviewMetrics::empty();
        let render =
            |paint| cs(&roots, &repo_deps, &aur_deps, &removals, &pac, &metrics).table(paint);

        let (plain, colored) = (render(Paint::Plain), render(Paint::Colored));

        // The arrow glyph is the one textual difference between the paints.
        assert_contains!(colored.lines()[0], "→");
        assert_not_contains!(colored.lines()[0], "->");
        assert_contains!(plain.lines()[0], "->");

        assert_eq!(colored.lines().len(), plain.lines().len());
        for (c, p) in colored.lines().iter().zip(plain.lines()) {
            let stripped = strip_ansi_codes(c);
            let colored_tokens: Vec<&str> = stripped
                .split_whitespace()
                .map(|t| if t == "→" { "->" } else { t })
                .collect();
            let plain_tokens: Vec<&str> = p.split_whitespace().collect();
            assert_eq!(
                colored_tokens, plain_tokens,
                "both paints must carry the same text content"
            );
        }
    }

    /// Every root row's size figure ends at the same visible column, in both
    /// paints — the version block must occupy one fixed width whether the row
    /// is an upgrade (with an arrow) or a fresh install (blank gap).
    /// Regression: the plain ` -> ` separator is four columns but the blank
    /// gap was sized by the colored arrow's three (the old `ARROW` const), so
    /// plain-mode carts mixing upgrades and fresh installs had every column
    /// after the version block off by one cell between rows.
    #[test]
    fn transaction_table_size_column_aligns_across_rows() {
        let mut pac = PacmanIndex::default();
        pac.sync_download_size.insert("glibc".into(), mib(12));
        pac.sync_download_size.insert("newpkg".into(), mib(5));
        pac.installed_size.insert("cuda".into(), gib(3));
        let roots = vec![
            root("core", "glibc", Some("2.40-1"), Some("2.41-1")),
            root("aur", "cuda", Some("12.6-1"), Some("12.8-1")),
            root("extra", "newpkg", None, Some("1.0-1")), // fresh: no arrow
        ];
        let size_re = regex::Regex::new(r"\d[\d.]* [KMGT]iB").unwrap();
        for paint in [Paint::Plain, Paint::Colored] {
            let table = cs(&roots, &[], &[], &[], &pac, &PreviewMetrics::empty()).table(paint);
            let end_columns: Vec<usize> = table.lines()[..3]
                .iter()
                .map(|line| {
                    let stripped = strip_ansi_codes(line);
                    let m = size_re
                        .find(&stripped)
                        .unwrap_or_else(|| panic!("no size figure in {stripped:?}"));
                    stripped[..m.end()].chars().count()
                })
                .collect();
            assert!(
                end_columns.windows(2).all(|w| w[0] == w[1]),
                "size column drifts across rows under {paint:?}: {end_columns:?}"
            );
        }
    }

    /// The one-line summary lists counts + size, omits the deps/remove/build
    /// terms when those are absent, and marks the size a lower bound (`>`) when
    /// an unknown-size row is in the mix.
    #[test]
    fn cost_summary_counts_and_terms() {
        let mut pac = PacmanIndex::default();
        pac.sync_download_size.insert("glibc".into(), 100);
        let roots = vec![root("core", "glibc", Some("1-1"), Some("1-2"))];
        let plain = cs(&roots, &[], &[], &[], &pac, &PreviewMetrics::empty()).summary();
        assert_eq!(plain, "1 install · 100 B");

        pac.installed_size.insert("cuda".into(), 1024);
        let roots = vec![
            root("core", "glibc", Some("1-1"), Some("1-2")),
            root("aur", "cuda", Some("1-1"), Some("2-1")),
        ];
        let mut metrics = PreviewMetrics::empty();
        metrics.root_build_secs.insert(PkgName::from("cuda"), 120);
        // `gcc13` has no syncdb size → an Unknown row → the size total is a
        // lower bound. `cuda` is measured (120s) with no unknown build in the
        // mix → an exact `2m 0s build`.
        let s = cs(
            &roots,
            &[PkgName::from("gcc13")],
            &[],
            &[PkgName::from("old")],
            &pac,
            &metrics,
        )
        .summary();
        assert!(
            s.starts_with("2 install, +1 dep, 1 remove · >"),
            "unknown-size dep makes the size a lower bound: {s}"
        );
        assert!(s.ends_with("· 2m 0s build"), "measured build term: {s}");
    }
}
