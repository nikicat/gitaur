//! Colored user-facing CLI output (banners, package lists, progress bars, prompts).
//!
//! Built on `console` (styling), `indicatif` (bars/spinners), and `dialoguer`
//! (prompts) — the pacman/yay-style UI stack. Independent of [`tracing`],
//! which carries diagnostic events for developers and stays silent unless
//! `RUST_LOG` enables it.
//!
//! Progress-bar conventions in this module:
//! - `{prefix}` carries the **fixed** row label (`objects`, `received`, …).
//! - `{msg}` / `{wide_msg}` carry **streaming** content (e.g. sideband lines).
//!
//! Splitting the two lets callers `set_message` without clobbering the label.

use crate::config::Config;
use crate::pacman::invoke::{PkgUpgrade, REPO_AUR};
use crate::pacman::verdiff::{self, BumpKind};

use console::{style, Term};
use dialoguer::theme::Theme;
use dialoguer::{Confirm, MultiSelect};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{BufRead, IsTerminal, Write};
use std::sync::OnceLock;
use std::time::Duration;

/// Tick frames used by every spinner row in this module.
const SPIN_TICKS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ";

/// Standard cadence for `enable_steady_tick`.
pub const TICK_PERIOD: Duration = Duration::from_millis(80);

/// Enable a steady tick at the canonical cadence. Always call this **after**
/// `MultiProgress::add(pb)` so the tick thread targets the `MultiProgress`
/// draw target — calling it before `add` produces phantom duplicate rows.
pub fn tick(pb: &ProgressBar) {
    pb.enable_steady_tick(TICK_PERIOD);
}

/// User preference for terminal color output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ColorMode {
    /// Detect TTY/`NO_COLOR`/etc. at print time.
    #[default]
    Auto,
    /// Force ANSI escapes on, even when stderr isn't a TTY.
    Always,
    /// Suppress all color escapes.
    Never,
}

static COLOR: OnceLock<ColorMode> = OnceLock::new();

/// Install the process-wide color mode. First caller wins.
pub fn set_color(mode: ColorMode) {
    let _ = COLOR.set(mode);
}

pub fn color_on() -> bool {
    match COLOR.get().copied().unwrap_or(ColorMode::Auto) {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => Term::stderr().features().colors_supported(),
    }
}

/// Print a top-level status line (`:: msg`) in bold blue.
pub fn info(msg: &str) {
    if color_on() {
        eprintln!("{} {}", style("::").bold().blue(), style(msg).bold());
    } else {
        eprintln!(":: {msg}");
    }
}

/// Print a build-phase banner (`==> msg`) in bold green.
pub fn step(msg: &str) {
    if color_on() {
        eprintln!("{} {}", style("==>").bold().green(), style(msg).bold());
    } else {
        eprintln!("==> {msg}");
    }
}

/// Print a warning line in yellow.
pub fn warn(msg: &str) {
    if color_on() {
        eprintln!("{} {}", style("warning:").yellow().bold(), msg);
    } else {
        eprintln!("warning: {msg}");
    }
}

/// Print an error line in red.
pub fn error(msg: &str) {
    if color_on() {
        eprintln!("{} {}", style("error:").red().bold(), msg);
    } else {
        eprintln!("error: {msg}");
    }
}

/// Print a detail/follow-up line in cyan.
pub fn note(msg: &str) {
    if color_on() {
        eprintln!("{} {}", style("->").cyan(), msg);
    } else {
        eprintln!("-> {msg}");
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
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UpgradeSelection {
    pub repo: Vec<String>,
    pub repo_skipped: Vec<String>,
    pub aur: Vec<String>,
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
/// or stdin is not a TTY — same UX rule as [`confirm`]. Default mask is
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
            (true, true) => sel.aur.push(u.name.clone()),
            (true, false) => {}
            (false, true) => sel.repo.push(u.name.clone()),
            (false, false) => sel.repo_skipped.push(u.name.clone()),
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
    let (old_pre, old_suf) = u.old_ver.split_at(cut);
    let (new_pre, new_suf) = u.new_ver.split_at(cut);
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

/// Y/n confirmation prompt with `Y` default. Honors `noconfirm` to auto-accept.
///
/// Falls back to a plain `stdin.read_line` when stdin is not a TTY so callers
/// can pipe an answer (`echo n | gitaur -S foo`), matching pacman/yay UX.
pub fn confirm(prompt: &str, noconfirm: bool) -> std::io::Result<bool> {
    if noconfirm {
        return Ok(true);
    }
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut out = std::io::stdout().lock();
        write!(out, "{prompt} [Y/n] ")?;
        out.flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(true);
        }
        let answer = line.trim();
        return Ok(!matches!(answer, "n" | "N" | "no" | "No" | "NO"));
    }
    Confirm::new()
        .with_prompt(prompt)
        .default(true)
        .interact()
        .map_err(std::io::Error::other)
}

/// Ask the user which pkgnames of a split pkgbase to install.
///
/// makepkg packages every pkgname of a split PKGBUILD in one go (there's no
/// flag to skip), but `gitaur` filters the resulting `.pkg.tar.zst` set
/// before `pacman -U` runs — so **unselected pkgnames are built but never
/// installed**. Selected pkgnames are installed as `Explicit`.
///
/// Short-circuits without prompting when:
///   * the pkgbase has a single pkgname (no real choice — just inform);
///   * `noconfirm` is set (auto-select every pkgname).
pub fn select_pkgnames(
    pkgbase: &str,
    pkgnames: &[String],
    noconfirm: bool,
) -> std::io::Result<Vec<String>> {
    if pkgnames.len() <= 1 {
        if let Some(only) = pkgnames.first() {
            if only != pkgbase {
                note(&format!("resolved pkgbase `{pkgbase}` → `{only}`"));
            }
        }
        return Ok(pkgnames.to_vec());
    }
    if noconfirm {
        return Ok(pkgnames.to_vec());
    }
    let chosen = MultiSelect::new()
        .with_prompt(format!(
            "[{pkgbase}] split package — pick pkgnames to install \
             (unselected are built but skipped at install time)"
        ))
        .items(pkgnames)
        .defaults(&vec![true; pkgnames.len()])
        .interact()
        .map_err(std::io::Error::other)?;
    Ok(chosen.into_iter().map(|i| pkgnames[i].clone()).collect())
}

/// Bounded-byte progress bar (used when a total is known up-front).
pub fn bar_bytes(total: u64, label: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_draw_target(ProgressDrawTarget::hidden());
    pb.set_style(bytes_active_style());
    pb.set_prefix(label.to_string());
    pb
}

/// Streaming byte counter with no known total (shows `received` + rate +
/// elapsed). Caller should `mp.add(pb)` then `ui::tick(&pb)`.
pub fn bar_bytes_streaming(label: &str) -> ProgressBar {
    let pb = ProgressBar::no_length();
    pb.set_draw_target(ProgressDrawTarget::hidden());
    pb.set_style(bytes_pending_style());
    pb.set_prefix(label.to_string());
    pb
}

/// Swap a pending byte bar (`bar_bytes_streaming`) to its active form
/// (`bar_bytes`) once a total becomes known.
pub fn promote_byte_bar(pb: &ProgressBar, total: u64) {
    if pb.length() != Some(total) {
        pb.set_length(total);
        pb.set_style(bytes_active_style());
    }
}

fn bytes_pending_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix:>14.cyan.bold} {spinner} [{elapsed:>4}] {bytes:>10} ({binary_bytes_per_sec})",
    )
    .unwrap()
    .tick_chars(SPIN_TICKS)
}

fn bytes_active_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix:>14.cyan.bold} {spinner} [{elapsed:>4}] {bytes:>10}/{total_bytes:<10} [{bar:20.green/dim}] {binary_bytes_per_sec} (eta {eta})",
    )
    .unwrap()
    .tick_chars(SPIN_TICKS)
    .progress_chars("##-")
}

/// Build a count-oriented progress bar (e.g. parallel index workers).
///
/// When `total == 0`, renders as a spinner-counter (correct UX while we
/// wait for the real total). Call [`promote_count_bar`] once you learn the
/// length to switch to a true progress bar. Caller should `mp.add(pb)`
/// then `ui::tick(&pb)` if the bar starts in pending mode.
pub fn bar_count(total: u64, label: &str) -> ProgressBar {
    let pb = if total == 0 {
        let pb = ProgressBar::no_length();
        pb.set_draw_target(ProgressDrawTarget::hidden());
        pb.set_style(count_pending_style());
        pb
    } else {
        let pb = ProgressBar::new(total);
        pb.set_draw_target(ProgressDrawTarget::hidden());
        pb.set_style(count_active_style());
        pb
    };
    pb.set_prefix(label.to_string());
    pb
}

/// Swap a pending [`bar_count`] over to the active style and set its length.
/// Idempotent: re-calling with the same total is a no-op.
pub fn promote_count_bar(pb: &ProgressBar, total: u64) {
    if pb.length() != Some(total) {
        pb.set_length(total);
        pb.set_style(count_active_style());
    }
}

fn count_pending_style() -> ProgressStyle {
    // `{elapsed}` runs from the bar's creation; reassures the user that work
    // is happening even when gix doesn't emit per-step events for this phase.
    ProgressStyle::with_template("{prefix:>14.cyan.bold} {spinner} [{elapsed:>4}] {pos:>10}")
        .unwrap()
        .tick_chars(SPIN_TICKS)
}

fn count_active_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix:>14.cyan.bold} {spinner} [{elapsed:>4}] {pos:>10}/{len:<10} [{bar:20.green/dim}] (eta {eta})",
    )
    .unwrap()
    .tick_chars(SPIN_TICKS)
    .progress_chars("##-")
}

/// Spinner with a fixed label, an elapsed-time indicator, and a streaming
/// `wide_msg` body. Used for the libgit2 sideband channel (server-side
/// `remote: Counting objects...` etc.) and other long-running phases.
///
/// Caller should `mp.add(pb)` then `ui::tick(&pb)` so the spinner animates.
pub fn bar_sideband(label: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_draw_target(ProgressDrawTarget::hidden());
    pb.set_style(
        ProgressStyle::with_template("{prefix:>14.cyan.bold} {spinner} [{elapsed:>4}] {wide_msg}")
            .unwrap()
            .tick_chars(SPIN_TICKS),
    );
    pb.set_prefix(label.to_string());
    pb
}

/// Generic tick spinner for unbounded indeterminate work.
pub fn spinner(label: &str) -> ProgressBar {
    bar_sideband(label)
}

// ---------------------------------------------------------------------------
// gix progress bridge

use gix::progress::prodash::progress::Step;
use gix::progress::{Count as GixCount, Id, MessageLevel, Unit};
use gix::{NestedProgress, Progress as GixProgressTrait};
use indicatif::MultiProgress;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex, MutexGuard};

/// Adapter implementing [`gix::Progress`] / [`gix::NestedProgress`] on top of
/// our indicatif bars.
///
/// One shared summary line carries gix's most-recent `message()`. Each
/// gix child that actually emits step progress (`init` or `set`/`inc_by`)
/// owns its **own** leaf bar — created lazily on first such call, removed
/// from the `MultiProgress` when the child drops. Result: phases that emit
/// nothing don't stack rows, and concurrent children (e.g. `remote` +
/// `read pack`) coexist on screen the way `git clone` shows them.
pub struct GixProgress {
    shared: Arc<Shared>,
    /// Sub-phase name this clone owns (used as fallback when gix never
    /// calls `set_name` after `init`).
    own_name: String,
    /// Whether `init` was called with `progress::bytes()`. Drives whether
    /// the leaf formats `{bytes}`/`{binary_bytes_per_sec}` or raw counts.
    own_unit_is_bytes: bool,
    /// Max recorded by `init`/`set_max`; applied to the leaf the first
    /// time it actually gets created.
    own_max: Option<u64>,
    /// This node's own leaf bar (lazy). `None` until the node actually
    /// reports step progress (`set` or `inc_by`); cleared from the
    /// `MultiProgress` on `Drop`. Nodes that only get `set_name`'d (root,
    /// intermediate ancestors) never spawn a leaf.
    leaf: Mutex<Option<ProgressBar>>,
}

/// State shared by every node in one progress tree.
struct Shared {
    multi: MultiProgress,
    summary: ProgressBar,
}

/// Detect a byte-unit by asking the unit to format its own label.
///
/// In prodash 31, `Bytes::display_unit` writes nothing (the suffix is baked
/// into the value via `bytesize::ByteSize`), while every count-style unit
/// (`Human`, `Range`) writes its name (`"objects"`, `"steps"`, ...). So an
/// empty `display_unit` output uniquely identifies bytes — no string
/// matching, no heuristic.
fn unit_is_bytes(unit: &Unit) -> bool {
    let mut s = String::new();
    let _ = unit.as_display_value().display_unit(&mut s, 0);
    s.is_empty()
}

impl GixProgress {
    /// Create a fresh adapter. Stages just the summary line; leaves spawn
    /// lazily as gix children emit progress.
    pub fn new(label: &str) -> Self {
        let mp = MultiProgress::new();
        let summary = mp.add(bar_sideband(label));
        summary.set_message("starting…");
        tick(&summary);
        Self {
            shared: Arc::new(Shared { multi: mp, summary }),
            own_name: String::new(),
            own_unit_is_bytes: false,
            own_max: None,
            leaf: Mutex::new(None),
        }
    }

    /// Clear all live bars. Intended for end-of-clone cleanup.
    pub fn finish(&self) {
        if let Some(pb) = self.leaf.lock().unwrap().take() {
            pb.finish_and_clear();
        }
        self.shared.summary.finish_and_clear();
    }

    fn set_summary(&self, msg: String) {
        self.shared.summary.set_message(msg);
    }

    fn lock_leaf(&self) -> MutexGuard<'_, Option<ProgressBar>> {
        self.leaf.lock().unwrap()
    }

    /// Create or replace this node's own leaf bar with the configured style.
    fn restart_leaf(&self, name: &str) {
        let pb = if self.own_unit_is_bytes {
            self.shared.multi.add(bar_bytes_streaming(leaf_label(name)))
        } else {
            self.shared.multi.add(bar_count(0, leaf_label(name)))
        };
        tick(&pb);
        let mut g = self.lock_leaf();
        if let Some(old) = g.replace(pb) {
            old.finish_and_clear();
        }
    }

    /// Ensure a leaf exists with the current style; called lazily by `set`/`inc_by`.
    /// Applies any `own_max` that `init`/`set_max` recorded earlier. Returns
    /// without creating anything for muted phases (e.g. server-sideband echo).
    fn ensure_leaf(&self) {
        if self.lock_leaf().is_some() {
            return;
        }
        if leaf_is_muted(&self.own_name) {
            return;
        }
        let name = if self.own_name.is_empty() {
            "phase".to_string()
        } else {
            self.own_name.clone()
        };
        self.restart_leaf(&name);
        if let Some(m) = self.own_max {
            if let Some(pb) = self.lock_leaf().as_ref() {
                if self.own_unit_is_bytes {
                    promote_byte_bar(pb, m);
                } else {
                    promote_count_bar(pb, m);
                }
            }
        }
    }

    fn update_leaf(&self, step: u64, max: Option<u64>) {
        self.ensure_leaf();
        let g = self.lock_leaf();
        if let Some(pb) = g.as_ref() {
            if let Some(m) = max {
                if self.own_unit_is_bytes {
                    promote_byte_bar(pb, m);
                } else {
                    promote_count_bar(pb, m);
                }
            }
            pb.set_position(step);
        }
    }
}

impl Drop for GixProgress {
    fn drop(&mut self) {
        if let Some(pb) = self.leaf.lock().unwrap().take() {
            pb.finish_and_clear();
        }
    }
}

/// Condense gix's long phase names into our fixed 14-wide prefix column.
fn leaf_label(name: &str) -> &str {
    match name.to_ascii_lowercase().as_str() {
        "receiving objects" => "objects",
        "indexing" | "resolving deltas" => "deltas",
        "decompressing" => "decompress",
        "read pack" => "pack",
        _ => name,
    }
}

/// Map known gix phase names to a one-line user-facing hint. The hint tells
/// the user what gix is *actually* doing and gives a rough ETA so the silent
/// phases don't look stuck. Returns `None` for unknown phases; in that case
/// the summary shows just the raw gix name.
///
/// ETAs are calibrated for `github.com/archlinux/aur` (~155 k refs, ~2 GiB
/// pack) on a residential connection; smaller repos finish faster.
fn phase_hint(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("handshake") {
        Some("TLS + HTTP smart-protocol setup")
    } else if lower == "authentication" {
        Some("authenticating with server")
    } else if lower == "list refs" {
        Some("downloading ref list (~20 s)")
    } else if lower.starts_with("negotiate") {
        Some("sending wants/haves to server")
    } else if lower == "receiving pack" {
        Some("server is packing objects, then streaming to us (~5–8 min)")
    } else if lower == "read pack" {
        Some("silent until server finishes packing (~3–5 min server-side, ~2–3 min stream)")
    } else if lower == "remote" {
        Some("server-side progress (counting / compressing objects)")
    } else if lower == "indexing" || lower == "resolving deltas" || lower == "resolving" {
        Some("local delta resolution (CPU-heavy, ~1–2 min)")
    } else if lower.starts_with("decompress") || lower == "decoding" {
        Some("decompressing pack entries")
    } else if lower == "sorting by id" {
        Some("sorting pack entries (brief)")
    } else if lower == "writing index file" {
        Some("writing pack index — finishing up")
    } else if lower == "create index file" {
        Some("building pack index")
    } else if lower.contains("fetch") {
        // After the last visible bar (Resolving), gix runs `update_refs` to
        // write every received ref to disk; that step emits no progress for
        // ~30 s – 2 min on a 155 k-ref mirror. So when we're back in the
        // outer "fetch" name with no active child bars, mention it.
        Some("finalizing — writing refs silently (~30 s – 2 min)")
    } else {
        None
    }
}

/// Phases whose progress is essentially noise we'd rather hide — the server's
/// sideband-translated "remote: Counting objects" / "remote: Compressing
/// objects" lines, which gix re-emits as a child whose name is the full server
/// string. The information is already visible in the summary row when the
/// message arrives; a dedicated bar with a 28-character prefix just breaks
/// alignment.
fn leaf_is_muted(name: &str) -> bool {
    name.starts_with("remote") || name.starts_with("remote:")
}

/// Render `text` as supporting/secondary UI text — mid-gray (color 244)
/// italic. Reads clearly without competing with the bright primary text.
/// Use for hint annotations, last-built timestamps, anything the eye should
/// *not* lock onto.
pub fn dim(text: impl AsRef<str>) -> console::StyledObject<String> {
    style(text.as_ref().to_string()).color256(244).italic()
}

/// Build the summary text for a phase name, appending the hint (dimmed) when
/// one exists. The phase name stays at full brightness so the eye locks onto
/// it; the hint is supporting context.
fn summary_with_hint(name: &str) -> String {
    match phase_hint(name) {
        Some(hint) => format!("{name} {}", dim(format!("— {hint}"))),
        None => name.to_string(),
    }
}

impl GixCount for GixProgress {
    fn set(&self, step: Step) {
        tracing::trace!(target: "gix_progress", phase = %self.own_name, step, "set");
        self.update_leaf(step as u64, None);
    }

    fn step(&self) -> Step {
        self.lock_leaf()
            .as_ref()
            .map_or(0, |pb| Step::try_from(pb.position()).unwrap_or(Step::MAX))
    }

    fn inc_by(&self, step: Step) {
        tracing::trace!(target: "gix_progress", phase = %self.own_name, step, "inc_by");
        self.ensure_leaf();
        if let Some(pb) = self.lock_leaf().as_ref() {
            pb.inc(step as u64);
        }
    }

    fn counter(&self) -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }
}

impl GixProgressTrait for GixProgress {
    fn init(&mut self, max: Option<Step>, unit: Option<Unit>) {
        self.own_unit_is_bytes = unit.as_ref().is_some_and(unit_is_bytes);
        self.own_max = max.map(|m| m as u64);
        // TRACE, not DEBUG: gix re-`init`s its internal progress nodes hundreds
        // of times per fetch as it walks pack indices / refs / etc. The real
        // state changes are `add_child` (new phase) and `message` (sideband).
        tracing::trace!(
            target: "gix_progress",
            phase = %self.own_name,
            ?max,
            is_bytes = self.own_unit_is_bytes,
            "init"
        );
        // Don't spawn a leaf yet. Many gix nodes call `init` once at startup
        // and then only emit `set_name` afterwards — those should never get
        // a row of their own. The leaf is created on the first `set`/`inc_by`.
        // If we already have a leaf and `init` is being called again to
        // declare a length (e.g. the sideband-translated "Counting objects"
        // line setting a max after earlier max=None messages), promote in
        // place so the bar style matches the new bound.
        if let (Some(m), Some(pb)) = (self.own_max, self.lock_leaf().as_ref()) {
            if self.own_unit_is_bytes {
                promote_byte_bar(pb, m);
            } else {
                promote_count_bar(pb, m);
            }
        }
    }

    fn unit(&self) -> Option<Unit> {
        None
    }

    fn max(&self) -> Option<Step> {
        self.lock_leaf()
            .as_ref()
            .and_then(|pb| pb.length().map(|x| Step::try_from(x).unwrap_or(Step::MAX)))
    }

    fn set_max(&mut self, max: Option<Step>) -> Option<Step> {
        self.own_max = max.map(|m| m as u64);
        if let Some(m) = max {
            // Only resize the bar if it already exists; don't spawn one here.
            if let Some(pb) = self.lock_leaf().as_ref() {
                if self.own_unit_is_bytes {
                    promote_byte_bar(pb, m as u64);
                } else {
                    promote_count_bar(pb, m as u64);
                }
            }
        }
        max
    }

    fn set_name(&mut self, name: String) {
        // Dedupe: gix re-emits the same name on every progress tick (e.g.
        // "remote: Counting objects" hundreds of times). Only the actual phase
        // *transitions* are interesting — those become DEBUG.
        if name != self.own_name {
            tracing::debug!(target: "gix_progress", old = %self.own_name, new = %name, "set_name");
        }
        self.set_summary(summary_with_hint(&name));
        self.own_name.clone_from(&name);
        if let Some(pb) = self.lock_leaf().as_ref() {
            pb.set_prefix(leaf_label(&name).to_string());
        }
    }

    fn name(&self) -> Option<String> {
        Some(self.own_name.clone())
    }

    fn id(&self) -> Id {
        *b"GITA"
    }

    fn message(&self, _level: MessageLevel, message: String) {
        tracing::debug!(target: "gix_progress", phase = %self.own_name, %message, "message");
        // Synthesized marker: gix emits no `set_name`/`add_child` for the
        // ~15–20s post-pack ref-update phase, so the log goes silent right
        // when most users start wondering if it's hung. The only event gix
        // does fire beforehand is this "read pack" wrap-up message — promote
        // it into an explicit "next phase begins" line.
        let synth_post_pack_marker = self.own_name == "read pack" && message.starts_with("done");
        self.set_summary(message);
        if synth_post_pack_marker {
            tracing::debug!(
                target: "gix_progress",
                "post-pack phase begins: updating refs and writing pack manifest (silent in gix)"
            );
        }
    }
}

impl NestedProgress for GixProgress {
    type SubProgress = Self;

    fn add_child(&mut self, name: impl Into<String>) -> Self::SubProgress {
        let name = name.into();
        tracing::debug!(target: "gix_progress", parent = %self.own_name, child = %name, "add_child");
        self.set_summary(summary_with_hint(&name));
        Self {
            shared: Arc::clone(&self.shared),
            own_name: name,
            own_unit_is_bytes: false,
            own_max: None,
            leaf: Mutex::new(None),
        }
    }

    fn add_child_with_id(&mut self, name: impl Into<String>, _id: Id) -> Self::SubProgress {
        self.add_child(name)
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

    /// The upgrade-table header is auxiliary information — it must render in
    /// the same gray-italic style as phase hints, never bold. Pins the ANSI
    /// codes so a refactor that re-bolds the header fails loudly.
    #[test]
    fn dim_is_italic_color244_not_bold() {
        let out = dim("Repo upgrades (3)").force_styling(true).to_string();
        assert!(
            out.contains("\u{1b}[38;5;244m"),
            "missing color 244: {out:?}"
        );
        assert!(out.contains("\u{1b}[3m"), "missing italic: {out:?}");
        assert!(
            !out.contains("\u{1b}[1m"),
            "header should not be bold: {out:?}"
        );
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
        let sorted: Vec<&str> = sort_for_display(&ups)
            .iter()
            .map(|u| u.name.as_str())
            .collect();
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
        let sorted: Vec<&str> = sort_for_display(&ups)
            .iter()
            .map(|u| u.name.as_str())
            .collect();
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
