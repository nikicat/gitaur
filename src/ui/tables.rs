//! Aligned pacman/yay-style tables: install plans and upgrade plans. The
//! interactive `-Syu` picker is gone (the shell's cart replaced it); the
//! change-set preview lives in [`super::change_set`] — it reuses [`Paint`],
//! [`sort_for_display`], [`col_widths`], and [`render_row`] (all `pub(super)`
//! for that reason).

use super::{color_on, dim};
use crate::names::PkgName;
use crate::pacman::invoke::PkgUpgrade;
use crate::pacman::verdiff::{self, BumpKind};
use crate::version::Version;

use console::style;
use std::ops::Add;

/// A terminal column width, measured in display cells.
///
/// A newtype over `usize` (not a bare integer) so a width can't be confused
/// with a row index or a package count, and so the pad-on-*visible*-width policy
/// — cells are padded by char count, never byte length, so embedded ANSI
/// escapes don't skew alignment — lives in one place. Shared by the unified
/// transaction table ([`super::change_set`]) and [`version_block`].
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Width(usize);

impl Width {
    pub(super) const ZERO: Self = Self(0);

    /// Visible width of a plain (un-colored) cell. Table cells are ASCII
    /// (names, versions, repo labels, sizes), so char count == display columns.
    pub(super) fn of(s: &str) -> Self {
        Self(s.chars().count())
    }

    /// The widest of a set of cell widths — i.e. the column width. [`Self::ZERO`]
    /// when the iterator is empty.
    pub(super) fn widest(widths: impl Iterator<Item = Self>) -> Self {
        widths.max().unwrap_or(Self::ZERO)
    }

    /// The raw cell count, for the `{:>n}` / `{:<n}` width slots in `format!`.
    pub(super) const fn cells(self) -> usize {
        self.0
    }

    /// `self` blank cells — the filler for a fixed-width column with no content.
    pub(super) fn blanks(self) -> String {
        " ".repeat(self.0)
    }

    /// The padding string needed to widen a cell of visible width `inner` to
    /// `self` (empty when `inner` already meets or exceeds `self`).
    pub(super) fn gap(self, inner: Self) -> String {
        " ".repeat(self.0.saturating_sub(inner.0))
    }
}

impl Add for Width {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

/// A rendered table cell: its (possibly ANSI-colored) display text plus the
/// visible width of the *plain* text.
///
/// The domain type for a single aligned cell — instead of passing a
/// `(plain, rendered)` pair around, the cell carries its own visible width so
/// [`Self::pad_to`] aligns correctly even when the text holds color escapes.
pub(super) struct Cell {
    text: String,
    width: Width,
}

impl Cell {
    /// A plain (uncolored) cell; its width is its visible char count.
    pub(super) fn plain(s: impl Into<String>) -> Self {
        let text = s.into();
        let width = Width::of(&text);
        Self { text, width }
    }

    /// A cell rendered from `plain`: `f` paints it only when `paint` is colored,
    /// and the cell remembers `plain`'s visible width either way.
    pub(super) fn paint(plain: &str, paint: Paint, f: impl FnOnce(&str) -> String) -> Self {
        Self {
            width: Width::of(plain),
            text: if paint.colored() {
                f(plain)
            } else {
                plain.to_owned()
            },
        }
    }

    /// Left-justify to `width`: the rendered text followed by trailing blanks to
    /// reach `width` visible columns.
    pub(super) fn pad_to(self, width: Width) -> String {
        format!("{}{}", self.text, width.gap(self.width))
    }
}

/// The rendered lines of a table, top to bottom.
///
/// A domain newtype over the raw `Vec<String>` so rendered table output isn't
/// passed around as an anonymous string list (which could be confused with, say,
/// a list of package names), and so "how a table is emitted" lives behind one
/// type. The shell prints each [`Self::lines`] entry; tests assert over them.
pub struct Table(Vec<String>);

impl Table {
    /// An empty table to build up with [`Self::push`] / [`Self::append`].
    pub(super) const fn new() -> Self {
        Self(Vec::new())
    }

    /// Append one rendered line.
    pub(super) fn push(&mut self, line: String) {
        self.0.push(line);
    }

    /// Append another table's lines, consuming it. Lets a renderer assemble a
    /// table from sub-sections (root rows, the deps block, removals) without
    /// dropping to a bare `Vec<String>`.
    pub(super) fn append(&mut self, other: Self) {
        self.0.extend(other.0);
    }

    /// The display lines, top to bottom.
    pub fn lines(&self) -> &[String] {
        &self.0
    }

    /// Whether the render produced no lines (e.g. an empty change set).
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Whether a rendered row carries ANSI color.
///
/// An explicit per-render argument rather than a re-read of [`color_on`], so
/// the change-set preview can render a plain form (for width measurement) and a
/// colored form from the same code path — and so tests can pin [`Paint::Plain`]
/// instead of inheriting whatever the ambient terminal supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paint {
    Plain,
    Colored,
}

impl Paint {
    pub(super) const fn colored(self) -> bool {
        matches!(self, Self::Colored)
    }

    /// The ambient paint for the process's terminal ([`color_on`]).
    pub fn detect() -> Self {
        Self::from(color_on())
    }

    /// The visible width of the version separator this paint renders: the
    /// arrow glyph flanked by one space each side. Paint-dependent because
    /// the glyph is — colored draws the one-column `→`, plain falls back to
    /// the two-column ASCII `->` (piped / `NO_COLOR` output stays ASCII). A
    /// method rather than a const so the blank-gap math on arrowless rows is
    /// derived from the same paint as the rendered separator and the two
    /// can't drift apart.
    pub(super) const fn arrow(self) -> Width {
        match self {
            Self::Plain => Width(4),
            Self::Colored => Width(3),
        }
    }
}

impl From<bool> for Paint {
    fn from(colored: bool) -> Self {
        if colored { Self::Colored } else { Self::Plain }
    }
}

/// Display a pacman-style grouped package list: `Packages (N) a-1.0  b-2.0`.
pub fn pkg_list(label: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    let header = format!("{} ({})", label, items.len());
    let body = items.join("  ");
    if color_on() {
        eprintln!("\n{}\n    {}\n", style(header).bold(), body);
    } else {
        eprintln!("\n{header}\n    {body}\n");
    }
}

/// Display an aligned install plan table:
///
/// ```text
/// Repo packages (explicit) (2)
///     firefox          110.0-1
///     vim              9.1-2
/// ```
///
/// Companion to [`upgrade_table`] for `-S <pkg>` plans — the rows here are
/// always fresh installs (anything already at the target version was dropped
/// by the resolver), so there's no `old -> new` arrow to draw. An empty
/// `version` (e.g. an AUR name we couldn't look up) renders the name alone.
pub fn install_table(label: &str, rows: &[(String, String)]) {
    if rows.is_empty() {
        return;
    }
    let name_w = rows.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let header = format!("{} ({})", label, rows.len());

    eprintln!();
    if color_on() {
        eprintln!("{}", dim(&header));
        for (name, ver) in rows {
            eprintln!(
                "    {name:<name_w$}  {ver}",
                name = name,
                ver = style(ver).green(),
            );
        }
    } else {
        eprintln!("{header}");
        for (name, ver) in rows {
            eprintln!("    {name:<name_w$}  {ver}");
        }
    }
    eprintln!();
}

/// Display an aligned, colorized upgrade table:
///
/// ```text
/// Upgrades (5)
///     core      glibc            2.40-1          ->  2.41-1
///     extra     neovim           0.10.0-1        ->  0.10.2-1
///     multilib  wine             9.20-1          ->  9.21-1
///     aur       paru-bin         2.0.0-1         ->  2.0.1-1
///     aur       neovim-git       0.10.0.r123-1   ->  0.10.0.r130-1
/// ```
///
/// Rows are grouped by `repo` (canonical Arch order — core → extra →
/// multilib → other → aur), then severity-descending within group. All four
/// columns are space-padded uniformly across the whole list so package names
/// align regardless of which repo they come from. Version cells dim their
/// common prefix and color the diverging suffix by [`BumpKind`] (epoch/major
/// red, minor yellow, patch green, pkgrel cyan).
pub fn upgrade_table(plan: &[PkgUpgrade]) {
    if plan.is_empty() {
        return;
    }
    let ordered = sort_for_display(plan);
    let (repo_w, name_w, old_w) = col_widths(&ordered);
    let header = format!("Upgrades ({})", ordered.len());

    eprintln!();
    let colored = color_on();
    let paint = Paint::from(colored);
    if colored {
        eprintln!("{}", dim(&header));
    } else {
        eprintln!("{header}");
    }
    for u in &ordered {
        eprintln!("    {}", render_row(u, repo_w, name_w, old_w, paint));
    }
    eprintln!();
}

/// The repo half of an `apply`'s upgrade transaction.
///
/// Built by the shell's `apply` (`repo_upgrade_selection`) and consumed by
/// [`crate::cli::dispatch::run_repo_upgrade`]: `repo` is the staged subset,
/// `repo_skipped` becomes the `--ignore=` list for the partial `pacman -Syu`.
/// `aur` is unused on this path (the AUR half goes through the build pipeline),
/// but kept so the type can also describe a full repo+AUR selection.
// No `Eq` — `PkgUpgrade.old_ver` / `new_ver` are `Version`, whose `PartialEq`
// is vercmp (not bytes-equal), and so doesn't satisfy `Eq`'s reflexivity
// guarantee in the bytes-distinct-but-vercmp-equal corner case. `Vec<_>` /
// HashMap usage doesn't rely on `Eq` here.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UpgradeSelection {
    pub repo: Vec<PkgName>,
    pub repo_skipped: Vec<PkgName>,
    pub aur: Vec<PkgUpgrade>,
}

impl UpgradeSelection {
    pub const fn is_empty(&self) -> bool {
        self.repo.is_empty() && self.aur.is_empty()
    }
}

/// Sort `plan` by (repo group, severity-descending, name) without copying.
/// The name tiebreaker keeps the table deterministic across runs — alpm's
/// localdb walk and the `HashMap`-backed foreign-pkg iterator both produce
/// non-stable input order, so a row's position would otherwise jitter
/// between invocations.
pub(super) fn sort_for_display(plan: &[PkgUpgrade]) -> Vec<&PkgUpgrade> {
    let mut rows: Vec<&PkgUpgrade> = plan.iter().collect();
    rows.sort_by(|a, b| {
        a.repo
            .rank()
            .cmp(&b.repo.rank())
            // Group same-rank `Other` repos by their concrete name; a no-op for
            // the canonical repos and AUR (constant name within a rank).
            .then_with(|| a.repo.as_str().cmp(b.repo.as_str()))
            .then_with(|| {
                verdiff::classify_bump(&a.old_ver, &a.new_ver)
                    .cmp(&verdiff::classify_bump(&b.old_ver, &b.new_ver))
            })
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

pub(super) fn col_widths(rows: &[&PkgUpgrade]) -> (usize, usize, usize) {
    let repo_w = rows.iter().map(|u| u.repo.len()).max().unwrap_or(0);
    let name_w = rows.iter().map(|u| u.name.len()).max().unwrap_or(0);
    let old_w = rows.iter().map(|u| u.old_ver.len()).max().unwrap_or(0);
    (repo_w, name_w, old_w)
}

/// Format one upgrade row at the given column widths. Shared by the static
/// `upgrade_table` and the change-set preview, so both stay visually identical.
pub(super) fn render_row(
    u: &PkgUpgrade,
    repo_w: usize,
    name_w: usize,
    old_w: usize,
    paint: Paint,
) -> String {
    if !paint.colored() {
        return format!(
            "{repo:<repo_w$}  {name:<name_w$}  {old:<old_w$}  ->  {new}",
            repo = u.repo,
            name = u.name,
            old = u.old_ver,
            new = u.new_ver,
        );
    }
    let kind = verdiff::classify_bump(&u.old_ver, &u.new_ver);
    let cut = verdiff::common_prefix_at_boundary(&u.old_ver, &u.new_ver);
    // Byte-level prefix/suffix split for the dim/bright color split — pure
    // UI concern, so `as_str()` is the explicit downgrade boundary.
    let (old_pre, old_suf) = u.old_ver.as_str().split_at(cut);
    let (new_pre, new_suf) = u.new_ver.as_str().split_at(cut);
    // Pad after splitting so trailing spaces ride with the (dim) prefix.
    let old_pad = " ".repeat(old_w.saturating_sub(u.old_ver.len()));
    let repo_pad = " ".repeat(repo_w.saturating_sub(u.repo.len()));
    format!(
        "{repo}{repo_pad}  {name:<name_w$}  {old_pre}{old_suf}{old_pad}  ->  {new_pre}{new_suf}",
        repo = super::repo(u.repo.as_str()),
        repo_pad = repo_pad,
        name = u.name,
        old_pre = style(old_pre).dim(),
        old_suf = style(old_suf).red(),
        old_pad = old_pad,
        new_pre = style(new_pre).dim(),
        new_suf = paint_suffix(new_suf, kind),
    )
}

fn paint_suffix(s: &str, kind: BumpKind) -> console::StyledObject<&str> {
    match kind {
        BumpKind::Epoch | BumpKind::Major => style(s).red().bold(),
        BumpKind::Minor => style(s).yellow().bold(),
        BumpKind::Patch => style(s).green(),
        BumpKind::PkgRel => style(s).cyan(),
        BumpKind::Other => style(s),
    }
}

/// Render one transaction row's version block, padded to a fixed
/// `old_w + paint.arrow() + new_w` visible width so the column after it
/// aligns across install and upgrade rows.
///
/// - **Upgrade** (`old` present): verdiff coloring — common prefix dimmed, the
///   diverging suffix colored by [`BumpKind`], joined by a dimmed ` → `. Shares
///   the exact split logic with [`render_row`] so the shell's transaction table
///   and the flag-path upgrade table read identically.
/// - **Fresh install** (`old` is `None`): the arrow is suppressed (blank gap)
///   and `new` renders green, matching [`install_table`].
/// - **Unknown version** (`new` is `None`): an all-blank block of the same
///   width, so a row we couldn't resolve a version for still aligns.
pub(super) fn version_block(
    old: Option<&Version>,
    new: Option<&Version>,
    old_w: Width,
    new_w: Width,
    paint: Paint,
) -> String {
    let Some(new) = new else {
        return (old_w + paint.arrow() + new_w).blanks();
    };
    let new_str = new.as_str();
    let new_pad = new_w.gap(Width::of(new_str));

    let Some(old) = old else {
        // Fresh install: blank old slot + blank arrow gap, then green `new`.
        let lead = (old_w + paint.arrow()).blanks();
        let shown = if paint.colored() {
            style(new_str).green().to_string()
        } else {
            new_str.to_owned()
        };
        return format!("{lead}{shown}{new_pad}");
    };

    let old_pad = old_w.gap(Width::of(old.as_str()));
    if !paint.colored() {
        return format!("{}{old_pad} -> {new_str}{new_pad}", old.as_str());
    }
    let kind = verdiff::classify_bump(old, new);
    let cut = verdiff::common_prefix_at_boundary(old, new);
    let (old_pre, old_suf) = old.as_str().split_at(cut);
    let (new_pre, new_suf) = new_str.split_at(cut);
    format!(
        "{}{}{old_pad}{}{}{}{new_pad}",
        style(old_pre).dim(),
        style(old_suf).red(),
        dim(" → "),
        style(new_pre).dim(),
        paint_suffix(new_suf, kind),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paint_suffix_dispatches_every_kind() {
        // Smoke-test the dispatch table: every BumpKind renders a string that
        // still contains the input text. Exact ANSI codes are an internal of
        // `console` and not worth pinning.
        for kind in [
            BumpKind::Epoch,
            BumpKind::Major,
            BumpKind::Minor,
            BumpKind::Patch,
            BumpKind::PkgRel,
            BumpKind::Other,
        ] {
            let s = paint_suffix("1.2.3", kind).force_styling(true).to_string();
            assert!(s.contains("1.2.3"), "{kind:?} dropped the text: {s:?}");
        }
    }

    /// `sort_for_display` is the single source of truth for upgrade-row order.
    /// Within one repo it must emit most-severe-first, then alphabetical-by-name
    /// for same-severity rows so the table is deterministic across runs (alpm
    /// and `HashMap` iterators give non-stable input order). Covers both
    /// `upgrade_table` and the picker.
    #[test]
    fn sort_for_display_severity_then_name() {
        // Input is deliberately scrambled — `patch-b` before `patch-a` — so
        // the assertion would fail if the sort fell back to input order.
        let ups = vec![
            PkgUpgrade {
                repo: "extra".into(),
                name: "patch-b".into(),
                old_ver: "2.3.4-1".into(),
                new_ver: "2.3.5-1".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "major".into(),
                old_ver: "1.0-1".into(),
                new_ver: "2.0-1".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "pkgrel".into(),
                old_ver: "1.0-1".into(),
                new_ver: "1.0-2".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "epoch".into(),
                old_ver: "1:1.0-1".into(),
                new_ver: "2:1.0-1".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "patch-a".into(),
                old_ver: "1.0.0-1".into(),
                new_ver: "1.0.1-1".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "minor".into(),
                old_ver: "1.0-1".into(),
                new_ver: "1.1-1".into(),
            },
        ];
        let sorted: Vec<&PkgName> = sort_for_display(&ups).iter().map(|u| &u.name).collect();
        assert_eq!(
            sorted,
            ["epoch", "major", "minor", "patch-a", "patch-b", "pkgrel"]
        );
    }

    /// Group ordering: core → extra → multilib → (other repos, alphabetical)
    /// → aur. Severity inside each group still applies.
    #[test]
    fn sort_for_display_groups_then_severity() {
        let ups = vec![
            PkgUpgrade {
                repo: "aur".into(),
                name: "aur-major".into(),
                old_ver: "1.0-1".into(),
                new_ver: "2.0-1".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "extra-patch".into(),
                old_ver: "1.0.0-1".into(),
                new_ver: "1.0.1-1".into(),
            },
            PkgUpgrade {
                repo: "core".into(),
                name: "core-pkgrel".into(),
                old_ver: "1.0-1".into(),
                new_ver: "1.0-2".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "extra-major".into(),
                old_ver: "1.0-1".into(),
                new_ver: "2.0-1".into(),
            },
            PkgUpgrade {
                repo: "multilib".into(),
                name: "ml-minor".into(),
                old_ver: "1.0-1".into(),
                new_ver: "1.1-1".into(),
            },
            PkgUpgrade {
                repo: "testing".into(),
                name: "testing-patch".into(),
                old_ver: "1.0.0-1".into(),
                new_ver: "1.0.1-1".into(),
            },
        ];
        let sorted: Vec<&PkgName> = sort_for_display(&ups).iter().map(|u| &u.name).collect();
        assert_eq!(
            sorted,
            [
                "core-pkgrel",
                "extra-major",
                "extra-patch",
                "ml-minor",
                "testing-patch",
                "aur-major",
            ]
        );
    }

    /// Empty version cells (provides-only matches) must not break the
    /// name-column padding or panic on the format machinery.
    #[test]
    fn install_table_smoke() {
        let rows = vec![
            ("short".to_owned(), "1.0-1".to_owned()),
            ("much-longer-name".to_owned(), "1.2.3-4".to_owned()),
            ("provides-only".to_owned(), String::new()),
        ];
        install_table("Test installs", &rows);
        install_table("Empty", &[]);
    }

    /// `upgrade_table` writes to stderr so we can't capture its output without
    /// process plumbing, but we *can* assert it doesn't panic on the cases
    /// most likely to break the padding/split math.
    #[test]
    fn upgrade_table_smoke() {
        let ups = vec![
            PkgUpgrade {
                repo: "core".into(),
                name: "short".into(),
                old_ver: "1.0-1".into(),
                new_ver: "1.0-2".into(),
            },
            PkgUpgrade {
                repo: "extra".into(),
                name: "much-longer-name".into(),
                old_ver: "1.2.3-1".into(),
                new_ver: "2.0.0-1".into(),
            },
            PkgUpgrade {
                repo: "aur".into(),
                name: "epochpkg".into(),
                old_ver: "1:1.0-1".into(),
                new_ver: "2:1.0-1".into(),
            },
        ];
        upgrade_table(&ups);
        upgrade_table(&[]);
    }
}
