//! Aligned pacman/yay-style tables: install plans, upgrade plans, and the
//! interactive upgrade picker that shares their formatting.

use super::{color_on, dim};
use crate::config::Config;
use crate::names::PkgName;
use crate::pacman::invoke::{PkgUpgrade, REPO_AUR};
use crate::pacman::verdiff::{self, BumpKind};

use console::style;
use dialoguer::theme::Theme;
use dialoguer::MultiSelect;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::IsTerminal;

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
    if colored {
        eprintln!("{}", dim(&header));
    } else {
        eprintln!("{header}");
    }
    for u in &ordered {
        eprintln!("    {}", render_row(u, repo_w, name_w, old_w, colored));
    }
    eprintln!();
}

/// User's choice from the interactive `-Syu` picker. Pkgnames split by where
/// the caller needs them: `repo` joins `pacman -Syu`'s subset, `repo_skipped`
/// becomes the `--ignore=` list, `aur` is the queue for `cmd_install`.
///
/// `aur` carries the full [`PkgUpgrade`] (not just pkgname) so the user's
/// installed-version + intent survive the picker → install boundary. The
/// install pipeline uses the foreign pkgname as the counterpart hint for
/// review labelling — without it, asking to install a pkgbase whose entry
/// declares many `provides=` (e.g. .NET's shared `aspnet-runtime` virtual)
/// would have to guess which installed pkg is the one the user meant.
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
    pub fn is_empty(&self) -> bool {
        self.repo.is_empty() && self.aur.is_empty()
    }
}

/// `dialoguer::Theme` shim that swaps each plain item back to its colored
/// rendering at draw time. dialoguer 0.11 measures wrap from `items[i].len()`
/// (raw bytes); feeding it ANSI-styled labels makes its redraw over-clear and
/// overwrite output above the prompt. We feed it plain labels and recolor
/// here — output strings aren't measured, so wrap math stays correct.
struct UpgradePickerTheme<'a> {
    colored: HashMap<&'a str, String>,
}

impl<'a> UpgradePickerTheme<'a> {
    fn new(colored: HashMap<&'a str, String>) -> Self {
        Self { colored }
    }
}

impl Theme for UpgradePickerTheme<'_> {
    fn format_multi_select_prompt_item(
        &self,
        f: &mut dyn fmt::Write,
        text: &str,
        checked: bool,
        active: bool,
    ) -> fmt::Result {
        let prefix = match (checked, active) {
            (true, true) => "> [x]",
            (true, false) => "  [x]",
            (false, true) => "> [ ]",
            (false, false) => "  [ ]",
        };
        let display = self.colored.get(text).map_or(text, String::as_str);
        write!(f, "{prefix} {display}")
    }
}

/// Render the upgrade plan as a `dialoguer::MultiSelect` and split the user's
/// selection into the three buckets `UpgradeSelection` carries.
///
/// Falls back to the default mask without prompting when `noconfirm` is set
/// or stdin is not a TTY — same UX rule as [`super::confirm`]. Default mask is
/// "repo rows checked, AUR rows per `cfg.aur_default_select`"; the AUR knob
/// lets users opt into yay/paru parity (everything pre-selected).
pub fn select_upgrades(
    plan: &[PkgUpgrade],
    cfg: &Config,
    noconfirm: bool,
) -> std::io::Result<UpgradeSelection> {
    if plan.is_empty() {
        return Ok(UpgradeSelection::default());
    }
    let ordered = sort_for_display(plan);
    let defaults: Vec<bool> = ordered.iter().map(|u| default_for(u, cfg)).collect();

    let interactive = !noconfirm && std::io::stdin().is_terminal();
    let selected: Vec<usize> = if interactive {
        let (repo_w, name_w, old_w) = col_widths(&ordered);
        let colored = color_on();
        // Pass plain-ASCII labels to dialoguer so its redraw
        // (`clear_preserve_prompt`) measures byte length ≈ visible width;
        // it counts bytes against terminal columns to estimate wrap and
        // would otherwise over-clear every redraw — eating lines above the
        // prompt — when items carry ANSI escapes. Colour is reapplied at
        // render time via [`UpgradePickerTheme`].
        let plain: Vec<String> = ordered
            .iter()
            .map(|u| render_row(u, repo_w, name_w, old_w, false))
            .collect();
        let theme = UpgradePickerTheme::new(if colored {
            ordered
                .iter()
                .zip(&plain)
                .map(|(u, p)| (p.as_str(), render_row(u, repo_w, name_w, old_w, true)))
                .collect()
        } else {
            HashMap::new()
        });
        MultiSelect::with_theme(&theme)
            .with_prompt("Select upgrades to apply (space toggles, a inverts, enter confirms)")
            .items(&plain)
            .defaults(&defaults)
            // Suppress dialoguer's post-interaction "report" line — it would
            // re-list every selected row as a single wrapped line, duplicating
            // the table the user just confirmed.
            .report(false)
            .interact()
            .map_err(std::io::Error::other)?
    } else {
        defaults
            .iter()
            .enumerate()
            .filter_map(|(i, &on)| on.then_some(i))
            .collect()
    };

    let picked: HashSet<usize> = selected.into_iter().collect();
    let mut sel = UpgradeSelection::default();
    for (i, u) in ordered.iter().enumerate() {
        let is_aur = u.repo == REPO_AUR;
        match (is_aur, picked.contains(&i)) {
            (true, true) => sel.aur.push((*u).clone()),
            (true, false) => {}
            (false, true) => sel.repo.push(u.name.clone()),
            (false, false) => sel.repo_skipped.push(u.name.clone()),
            // (sel.repo / repo_skipped are typed `Vec<PkgName>`; the
            //  pacman -Syu --ignore boundary in `run_repo_upgrade` joins
            //  them via slice::join which routes through `Borrow<str>`.)
        }
    }
    Ok(sel)
}

fn default_for(u: &PkgUpgrade, cfg: &Config) -> bool {
    if u.repo == REPO_AUR {
        cfg.aur_default_select
    } else {
        true
    }
}

/// Sort `plan` by (repo group, severity-descending, name) without copying.
/// The name tiebreaker keeps the table deterministic across runs — alpm's
/// localdb walk and the `HashMap`-backed foreign-pkg iterator both produce
/// non-stable input order, so a row's position would otherwise jitter
/// between invocations.
fn sort_for_display(plan: &[PkgUpgrade]) -> Vec<&PkgUpgrade> {
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

fn col_widths(rows: &[&PkgUpgrade]) -> (usize, usize, usize) {
    let repo_w = rows.iter().map(|u| u.repo.len()).max().unwrap_or(0);
    let name_w = rows.iter().map(|u| u.name.len()).max().unwrap_or(0);
    let old_w = rows.iter().map(|u| u.old_ver.len()).max().unwrap_or(0);
    (repo_w, name_w, old_w)
}

/// Format one upgrade row at the given column widths. Shared by the static
/// `upgrade_table` and the interactive picker so both stay visually identical.
fn render_row(u: &PkgUpgrade, repo_w: usize, name_w: usize, old_w: usize, colored: bool) -> String {
    if !colored {
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
        repo = style(&u.repo).color256(244),
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

    /// AUR rows default off (opt-in), repo rows default on. Toggling the
    /// config knob flips the AUR side without touching repo behavior.
    #[test]
    fn default_for_respects_aur_knob() {
        use crate::config::defaults::default_config;
        let cfg_off = default_config();
        let mut cfg_on = cfg_off.clone();
        cfg_on.aur_default_select = true;
        let repo = PkgUpgrade {
            repo: "extra".into(),
            name: "vim".into(),
            old_ver: "1-1".into(),
            new_ver: "1-2".into(),
        };
        let aur = PkgUpgrade {
            repo: REPO_AUR.into(),
            name: "paru-bin".into(),
            old_ver: "1-1".into(),
            new_ver: "1-2".into(),
        };
        assert!(default_for(&repo, &cfg_off));
        assert!(default_for(&repo, &cfg_on));
        assert!(!default_for(&aur, &cfg_off));
        assert!(default_for(&aur, &cfg_on));
    }

    /// Empty version cells (provides-only matches) must not break the
    /// name-column padding or panic on the format machinery.
    #[test]
    fn install_table_smoke() {
        let rows = vec![
            ("short".to_string(), "1.0-1".to_string()),
            ("much-longer-name".to_string(), "1.2.3-4".to_string()),
            ("provides-only".to_string(), String::new()),
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

    /// `UpgradePickerTheme` must never hand dialoguer an ANSI-bearing string
    /// for an item: that's the whole reason the theme exists. Items always
    /// arrive plain; the theme paints them on the way out. Cover both the
    /// hit (key present in the map → colored output) and the miss (key
    /// absent → plain fallback), plus every `(checked, active)` prefix.
    #[test]
    fn picker_theme_paints_known_rows_and_falls_back_for_unknown() {
        let mut colored = HashMap::new();
        colored.insert(
            "extra  vim",
            "\u{1b}[38;5;244mextra\u{1b}[0m  vim".to_string(),
        );
        let theme = UpgradePickerTheme::new(colored);

        let mut buf = String::new();
        theme
            .format_multi_select_prompt_item(&mut buf, "extra  vim", true, true)
            .unwrap();
        assert!(buf.starts_with("> [x] "), "wrong prefix: {buf:?}");
        assert!(
            buf.contains("\u{1b}[38;5;244m"),
            "colored mapping was not applied: {buf:?}"
        );

        let mut buf = String::new();
        theme
            .format_multi_select_prompt_item(&mut buf, "aur  unmapped", false, false)
            .unwrap();
        assert_eq!(buf, "  [ ] aur  unmapped");
    }

    /// All four `(checked, active)` cells must emit the prefix dialoguer's
    /// `SimpleTheme` would emit — that's the contract we replaced, and the
    /// regression test for it is that the cursor + checkbox glyphs stay
    /// where the user expects them.
    #[test]
    fn picker_theme_prefix_matrix() {
        let theme = UpgradePickerTheme::new(HashMap::new());
        for (checked, active, expected) in [
            (true, true, "> [x] "),
            (true, false, "  [x] "),
            (false, true, "> [ ] "),
            (false, false, "  [ ] "),
        ] {
            let mut buf = String::new();
            theme
                .format_multi_select_prompt_item(&mut buf, "x", checked, active)
                .unwrap();
            assert!(
                buf.starts_with(expected),
                "checked={checked} active={active} → {buf:?} (want prefix {expected:?})"
            );
        }
    }
}
