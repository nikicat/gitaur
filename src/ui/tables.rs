//! Aligned pacman/yay-style tables: install plans and upgrade plans. The
//! interactive `-Syu` picker is gone (the shell's cart replaced it); the
//! change-set preview lives in [`super::change_set`] — it reuses [`Paint`],
//! [`sort_for_display`], [`col_widths`], and [`render_row`] (all `pub(super)`
//! for that reason).

use super::{color_on, dim};
use crate::names::PkgName;
use crate::pacman::invoke::{PkgUpgrade, REPO_AUR};
use crate::pacman::verdiff::{self, BumpKind};

use console::style;

/// Whether a rendered row carries ANSI color.
///
/// An explicit per-render argument rather than a re-read of [`color_on`], so
/// the change-set preview can render a plain form (for width measurement) and a
/// colored form from the same code path.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Paint {
    Plain,
    Colored,
}

impl Paint {
    pub(super) const fn colored(self) -> bool {
        matches!(self, Self::Colored)
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
        repo_rank(&a.repo)
            .cmp(&repo_rank(&b.repo))
            .then_with(|| {
                verdiff::classify_bump(&a.old_ver, &a.new_ver)
                    .cmp(&verdiff::classify_bump(&b.old_ver, &b.new_ver))
            })
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

/// Sort key for `repo`. Pinned positions for the three canonical Arch repos
/// and AUR last; any other configured repo (testing, custom, ...) lands in
/// between and breaks ties alphabetically.
fn repo_rank(repo: &str) -> (u8, &str) {
    match repo {
        "core" => (0, ""),
        "extra" => (1, ""),
        "multilib" => (2, ""),
        REPO_AUR => (255, ""),
        other => (10, other),
    }
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
        repo = super::repo(&u.repo),
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
