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

use crate::error::Result;
use crate::index;
use crate::mirror;
use crate::names::{PkgBase, PkgTarget, RepoName, SearchTerm};
use crate::pacman::invoke::PkgUpgrade;
use crate::system;
use crate::ui;
use crate::units::ByteSize;
use cart::{ApplyOutcome, AurApproval, Cart, ReviewOutcome, StageClass};

pub mod cart;
pub mod command;
pub mod complete;
mod env;
mod help;
mod repl;
pub mod selector;
mod staging;
#[cfg(test)]
mod testenv;
pub mod upgrade;
mod verbs;

pub use repl::run;

/// One row of a numbered list (search results or the cart), addressable by its
/// 1-based number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// The thing `add` / `info` / … act on when this row is picked by number.
    pub target: PkgTarget,
    /// Preformatted display label (without the leading number).
    pub label: String,
    /// Repo bucket (`core`, `extra`, …, or `aur`) this row came from, so a
    /// repo-name selector (`add extra`) can filter the list. `None` for rows
    /// whose source isn't a repo (e.g. cart-derived selector lists).
    pub repo: Option<RepoName>,
}

/// Which numbered list a bare number (`3`, `2-4`) currently indexes.
///
/// The shell prints two kinds of numbered table — search results and the staged
/// transaction — and a number always means the row you last brought up. `search`
/// switches to [`View::Search`]; the verbs that bring the transaction to the
/// foreground (`show`, `upgrade`, `drop`, `keep`, `undo`) switch to
/// [`View::Cart`]. The list verbs (`add`, `remove`, `info`) read the active list
/// but leave the view alone, so working through a search list with a run of
/// `add`s keeps the numbers pointing at that list even though each `add`
/// reprints the cart.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum View {
    /// Numbers index the most recent `search` result list.
    #[default]
    Search,
    /// Numbers index the staged cart rows (as `show` prints them).
    Cart,
}

/// How deep the `undo` stack goes — plenty for an interactive session, bounded
/// so a long-running shell can't grow it without limit.
const UNDO_DEPTH: usize = 64;

/// Mutable per-session shell state the dispatch core threads between commands.
#[derive(Default)]
pub struct State {
    /// The most recent `search` result list, indexed by number while the search
    /// view is active (see [`View`]).
    search_list: Vec<ListItem>,
    /// Which list bare numbers currently address — search results or the cart.
    view: View,
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

/// The side-effecting operations command dispatch needs.
///
/// Behind a trait so the pure control flow ([`State::dispatch`]) is unit-testable
/// with a scripted fake. The cart mutations stay on [`State`]; this trait is the
/// I/O seam (search, classification, the PKGBUILD diff, the build+install).
pub trait ShellEnv {
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
    /// Run a combined repo + AUR search; returns rows for the numbered list.
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
    /// Run the PKGBUILD review (diff-or-full) for one staged AUR target.
    fn review(&mut self, target: &PkgTarget) -> Result<ReviewOutcome>;
    /// Render the staged transaction table — the numbered install rows + the
    /// removal rows — colored, column-aligned, with a per-AUR-row "last
    /// modified" age. The header + approval summary stay in the pure dispatch
    /// core ([`State::show`]); this is the I/O-shaped presentation (color,
    /// width math, wall-clock age) that belongs behind the env seam.
    fn render_cart(&mut self, cart: &Cart);
    /// Run the staged transaction: resolve + preview + confirm + build/install +
    /// removals. Reads the cart; the dispatch core updates it from the outcome.
    fn apply(&mut self, cart: &Cart) -> Result<ApplyOutcome>;
    /// Measure aurox's on-disk state per category, for `system show`.
    /// Infallible: missing/unreadable paths report as zero.
    fn system_usage(&mut self) -> system::Report;
    /// `system prune`: delete the re-derivable caches (mirror, index, sync
    /// dbs, build trees) behind a y/N confirm. `Ok(None)` = user declined.
    /// Returns the bytes freed. The in-memory AUR data stays loaded — search
    /// and info keep working from it until a `refresh aur` re-fetches the
    /// mirror + index.
    fn system_prune(&mut self) -> Result<Option<ByteSize>>;
}
