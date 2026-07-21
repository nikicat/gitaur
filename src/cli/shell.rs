//! Interactive shell (REPL) for the no-arg `aurox` invocation.
//!
//! A persistent prompt the user drives with word-commands (`search`, `add`,
//! `upgrade`, `apply`, …) against long-lived session state, replacing the
//! wizard-style `dialoguer` flows. See `docs/plans/shell-ui.md` for the full
//! design and phasing.
//!
//! **Phase 4 status:** the session is hoisted at start (the AUR index +
//! lookup maps via [`AurIndexData`](crate::index::AurIndexData), a sorted name universe for
//! globs/completion, and the sync-repo name set for coarse classification) and
//! is *reloaded* on `upgrade`. The cart is live: `add` / `drop` / `remove` /
//! `clear` stage a [`cart::Cart`]; `upgrade` refreshes + seeds the available
//! upgrades (repo approved / AUR needs-review); `review` / `approve` move AUR
//! items past the approval gate; `show` previews it; `apply` gates on
//! all-approved, then runs the partial `pacman -Syu` repo lane + the AUR
//! build/install + `pacman -R` removals, with the cost-overlay change-set
//! preview ([`upgrade`]). This replaced the old `upgrade_loop` driver +
//! dialoguer picker. `refresh [aur|pacman]` re-fetches the package data
//! (both halves, or one) and reloads the session without touching the cart.
//!
//! The [`ShellEnv`]/[`State::dispatch`] split keeps command handling
//! unit-testable with a scripted fake: the side-effecting I/O (classification,
//! the PKGBUILD diff, the refresh+recompute, the build) lives behind the trait
//! so the cart mutations and the approval gate are exercised without a
//! terminal, index, or `makepkg`.

use crate::config::ConfigPath;
use crate::error::Result;
use crate::index;
use crate::mirror;
use crate::names::{PkgBase, PkgTarget, RepoName, SearchTerm};
use crate::pacman::invoke::PkgUpgrade;
use crate::system;
use crate::ui;
use crate::units::ByteSize;
use cart::{ApplyRun, AurApproval, Cart, ReviewOutcome, StageClass};
use resolved::ResolvedCart;

pub mod cart;
pub mod command;
pub mod complete;
mod env;
mod help;
mod repl;
pub mod resolved;
pub mod selector;
mod staging;
#[cfg(test)]
mod testenv;
pub mod upgrade;
mod verbs;

pub use repl::run;

/// One row of a numbered list (search results or the cart), addressable by its
/// 1-based number.
///
/// Pure selector data — no rendered text. The displayed table travels as a
/// [`ui::Table`] beside the list (same index order), so what's printed and
/// what a number resolves to come from the same rows without the rows ever
/// storing presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// The thing `add` / `info` / … act on when this row is picked by number.
    pub target: PkgTarget,
    /// Repo bucket (`core`, `extra`, …, or `aur`) this row came from, so a
    /// repo-name selector (`add extra`) can filter the list. `None` for rows
    /// whose source isn't a repo (e.g. cart-derived selector lists).
    pub repo: Option<RepoName>,
}

/// A cart row as a selector/list row — the shape `show` snapshots into the
/// referent. The conversion owns its clones once, at this named seam, instead
/// of scattering per-field `.clone()`s over the call sites.
impl From<&cart::CartItem> for ListItem {
    fn from(it: &cart::CartItem) -> Self {
        Self {
            target: it.spec().clone(),
            repo: Some(it.repo_label()),
        }
    }
}

/// Which numbered table a [`NumberedList`] snapshot came from — wording only
/// (error messages name the list a number was resolved against).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListSource {
    /// The `search` result table.
    Search,
    /// The transaction table (`show`, and the verbs that print through it).
    Transaction,
    /// `upgrade <sel>`'s freshly computed candidate list (selector-only; the
    /// table itself prints after the subset is seeded).
    UpgradeCandidates,
}

impl ListSource {
    /// How error messages name the list ("no row 9 — the {label} has 3 rows").
    const fn label(self) -> &'static str {
        match self {
            Self::Search => "search list",
            Self::Transaction => "transaction",
            Self::UpgradeCandidates => "upgrade candidates",
        }
    }
}

/// The rows of the last *numbered* table printed — what a bare number (`3`,
/// `2-4`) addresses. WYSIWYG addressing: a number is a name for a row the user
/// can see; output printed without row numbers is not addressable and leaves
/// this untouched.
///
/// Set only at the sites that render a numbered table (`search`, `show` — and
/// `upgrade`/`undo`/`redo`/the apply-failure path, which print through `show`),
/// so the screen and the addressable list cannot drift. A **snapshot**: a
/// number keeps naming *the package shown at that row* even after the cart
/// re-sorts or shrinks, so working down a printed table (`show`, `drop 2`,
/// `drop 4`) hits exactly the rows the user read — a since-dropped row is a
/// clean miss, never a silent wrong hit.
struct NumberedList {
    source: ListSource,
    rows: Vec<ListItem>,
}

/// How deep the `undo` stack goes — plenty for an interactive session, bounded
/// so a long-running shell can't grow it without limit.
const UNDO_DEPTH: usize = 64;

/// Mutable per-session shell state the dispatch core threads between commands.
#[derive(Default)]
pub struct State {
    /// The last numbered table printed, or `None` before any was — what bare
    /// numbers address (see [`NumberedList`]).
    referent: Option<NumberedList>,
    /// The staged transaction `apply` runs.
    cart: Cart,
    /// Pre-change cart snapshots for `undo`, most-recent last. Each cart-changing
    /// command pushes the cart as it was before the change; `undo` pops the top.
    history: Vec<Cart>,
    /// Carts popped by `undo`, for `redo` to replay. Cleared by any fresh
    /// cart-changing command — a new edit forks a new branch, so the undone
    /// future is discarded (standard undo/redo semantics).
    redo: Vec<Cart>,
}

/// Control-flow result of dispatching one command.
#[derive(Debug, PartialEq, Eq)]
pub enum Flow {
    /// Keep reading commands.
    Continue,
    /// Leave the shell with this process exit code.
    Exit(u8),
}

/// Whether a cart edit actually changed anything — the outcome the undoable-edit
/// seam ([`State::edit_cart`] / [`State::edit_and_resolve`]) keys on: a change
/// pushes an undo snapshot (and re-freezes the resolution), a no-op consumes no
/// undo step and stays quiet. A named outcome rather than a bare `bool`,
/// matching the cart's other result enums ([`cart::StageResult`] &c.).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CartEdit {
    /// The edit changed the cart (staged/unstaged/approved something).
    Changed,
    /// A no-op edit: everything was already staged, nothing matched, or the
    /// user's selection dropped no rows.
    Unchanged,
}

impl CartEdit {
    /// Lift a "did it change" bool — the leaf edits tally over a loop (a per-item
    /// flag, or a count ending in `n > 0`) and convert once at the return.
    const fn from_changed(changed: bool) -> Self {
        if changed {
            Self::Changed
        } else {
            Self::Unchanged
        }
    }

    /// Fold two edits — a batch verb (`remove <a> <b>`) runs several; any single
    /// change makes the whole batch [`Changed`](Self::Changed).
    const fn or(self, other: Self) -> Self {
        match (self, other) {
            (Self::Unchanged, Self::Unchanged) => Self::Unchanged,
            _ => Self::Changed,
        }
    }
}

/// The side-effecting operations command dispatch needs.
///
/// Behind a trait so the pure control flow ([`State::dispatch`]) is unit-testable
/// with a scripted fake. The cart mutations stay on [`State`]; this trait is the
/// I/O seam (search, classification, the PKGBUILD diff, the build+install).
pub(crate) trait ShellEnv {
    /// Emit one line of user-facing output.
    fn print(&mut self, line: &str);
    /// Emit a rendered table, line by line, through [`Self::print`] — the one
    /// place a [`ui::Table`] meets the shell's output seam.
    fn print_table(&mut self, table: &ui::Table) {
        for line in table.lines() {
            self.print(line);
        }
    }
    /// Refresh the mirror + index, reload the session (so `search`/`info` see
    /// fresh data too), and return the current upgrade candidates (repo ∪ AUR)
    /// for `upgrade` to seed into the cart.
    fn upgrade(&mut self) -> Result<Vec<PkgUpgrade>>;
    /// Re-fetch the package data `scope` covers and reload the session (fresh
    /// data for `search`/`info`/classification/completion) **without** seeding
    /// the cart — `upgrade` is the stage-the-upgrades variant; `refresh` is
    /// just the re-fetch. The outcome says whether the AUR half actually
    /// refreshed or was skipped (never set up / AUR disabled / out of scope),
    /// so the dispatch core can word the result.
    fn refresh(&mut self, scope: mirror::RefreshScope) -> Result<mirror::RefreshOutcome>;
    /// Run a combined repo + AUR search and print the numbered result table
    /// (rendering is env-side, like [`Self::render_cart`] — it needs the live
    /// pacman DBs and paint); returns the selector rows the printed numbers
    /// key. Prints nothing on no hits — the dispatch core words that case.
    fn search(&mut self, terms: &[SearchTerm]) -> Result<Vec<ListItem>>;
    /// Print `-Si`-style info for the already-resolved targets.
    fn show_info(&mut self, targets: &[PkgTarget]) -> Result<()>;
    /// Sorted universe of package targets, for glob resolution + completion.
    fn names(&self) -> &[PkgTarget];
    /// Coarse-classify a target for staging: a sync-repo package (with its
    /// concrete repo), an AUR package, or `None` when it's neither (a typo /
    /// unknown name). Only decides the approval policy and the `show` label —
    /// the real install routing is the resolver's call at `apply`.
    fn classify(&self, target: &PkgTarget) -> Option<StageClass>;
    /// Whether AUR items stage pre-approved — the effective `aur_approval`
    /// policy (see [`AurApproval::from_config`](cart::AurApproval::from_config)).
    fn aur_policy(&self) -> AurApproval;
    /// Where the AUR half stands this session — wording only (e.g. `add`'s
    /// unknown-name nudge); data flow stays uniform through the empty index.
    fn aur_state(&self) -> index::AurState;
    /// The pkgbase a staged AUR target resolves to, for the reviewed set fed
    /// into the build pipeline. `None` when it isn't a known AUR package.
    fn pkgbase_of(&self, target: &PkgTarget) -> Option<PkgBase>;
    /// Whether a persistent review approval (a prior session's consent) covers
    /// `target` at the PKGBUILD commit the index currently points at — the
    /// staging-time consult that lets an already-reviewed version stage
    /// pre-approved. `false` for unknown/non-AUR targets and when the store
    /// can't answer.
    fn previously_approved(&self, target: &PkgTarget) -> bool;
    /// Persist `target`'s approval at the index's current PKGBUILD commit —
    /// the durable half of the reviewed set. Infallible at this seam: the env
    /// degrades a store failure to a warning (one future re-prompt at worst).
    fn record_approval(&mut self, target: &PkgTarget);
    /// Run the PKGBUILD review (diff-or-full) for one staged AUR target.
    fn review(&mut self, target: &PkgTarget) -> Result<ReviewOutcome>;
    /// Resolve the whole cart — every staged install root (each carrying its
    /// [`SourcePin`](crate::build::SourcePin) so a namesake can't re-hijack it)
    /// plus the staged removals — into a frozen [`ResolvedCart`]: the plans
    /// `apply` executes plus the synced snapshot `show` renders against, in one
    /// add-time pass. The split-package picker is seeded from the cart's
    /// existing [`resolution`](Cart::resolution) so an already-resolved split
    /// root returns its stored choice without re-prompting. Propagates the
    /// resolver `Err` (a missing dep, a cycle) and a declared-conflict `Err` so
    /// the caller can **reject** the change and roll the cart back — an
    /// incoherent cart is never stored.
    fn stage_plan(&self, cart: &Cart) -> Result<ResolvedCart>;
    /// Render the staged transaction table — the numbered install rows + the
    /// removal rows — colored, column-aligned, with a per-AUR-row "last
    /// modified" age. The header + approval summary stay in the pure dispatch
    /// core ([`State::show`]); this is the I/O-shaped presentation (color,
    /// width math, wall-clock age) that belongs behind the env seam.
    fn render_cart(&mut self, cart: &Cart);
    /// Run the staged transaction: resolve + preview + confirm + build/install +
    /// removals. Reads the cart; the dispatch core updates it from the returned
    /// [`ApplyRun`] — the outcome plus the run's review knowledge, which the
    /// core folds back so mid-run approvals survive a failed run's retry.
    fn apply(&mut self, cart: &Cart) -> Result<ApplyRun>;
    /// Measure aurox's on-disk state per category, for `system show`.
    /// Infallible: missing/unreadable paths report as zero.
    fn system_usage(&mut self) -> system::Report;
    /// `system prune`: delete the re-derivable caches (mirror, index, sync
    /// dbs, build trees) behind a y/N confirm. `Ok(None)` = user declined.
    /// Returns the bytes freed. The in-memory AUR data stays loaded — search
    /// and info keep working from it until a `refresh aur` re-fetches the
    /// mirror + index.
    fn system_prune(&mut self) -> Result<Option<ByteSize>>;
    /// `config show [path]`: render the effective config (all knobs, one knob,
    /// or a whole `[section]`) as the current/default table and print it — the
    /// coloring of changed values is presentation, so it stays env-side (like
    /// [`Self::render_cart`]). `Err` for an unknown path (the core prefixes it).
    fn config_show(&mut self, path: Option<&ConfigPath>) -> Result<()>;
    /// `config set <path> <value…>`: validate the knob + value against the
    /// schema and persist the change (disk + in-memory view together). Returns a
    /// one-line summary; `Err` for an unknown path or a value the schema rejects
    /// (leaving the file untouched).
    fn config_set(&mut self, path: &ConfigPath, value: &[String]) -> Result<String>;
    /// `config reset <path>`: drop the user's override so the knob follows the
    /// built-in default again (sparse persistence — a now-empty section is
    /// pruned). Returns a one-line summary.
    fn config_reset(&mut self, path: &ConfigPath) -> Result<String>;
}
