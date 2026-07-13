//! Interactive shell (REPL) for the no-arg `aurox` invocation.
//!
//! A persistent prompt the user drives with word-commands (`search`, `add`,
//! `upgrade`, `apply`, …) against long-lived session state, replacing the
//! wizard-style `dialoguer` flows. See `docs/plans/shell-ui.md` for the full
//! design and phasing.
//!
//! **Phase 4 status:** the session is hoisted at start (the AUR index +
//! secondary maps via [`UpgradeSession`], a sorted name universe for
//! globs/completion, and the sync-repo name set for coarse classification) and
//! is *reloaded* on `upgrade`. The cart is live: `add` / `drop` / `remove` /
//! `clear` stage a [`cart::Cart`]; `upgrade` refreshes + seeds the available
//! upgrades (repo approved / AUR needs-review); `review` / `approve` move AUR
//! items past the approval gate; `show` previews it; `apply` gates on
//! all-approved, then runs the partial `pacman -Syu` repo lane + the AUR
//! build/install + `pacman -R` removals, with the cost-overlay change-set
//! preview ([`upgrade`]). This replaced the old `upgrade_loop` driver +
//! dialoguer picker. `refresh` lands in phase 5.
//!
//! The [`ShellEnv`]/[`State::dispatch`] split keeps command handling
//! unit-testable with a scripted fake: the side-effecting I/O (classification,
//! the PKGBUILD diff, the refresh+recompute, the build) lives behind the trait
//! so the cart mutations and the approval gate are exercised without a
//! terminal, index, or `makepkg`.

use crate::build::{self, ConfirmGate, DevelPolicy, InstallOpts, UpgradeSession, review};
use crate::cli::dispatch;
use crate::cli::search::{Row, rank_rows, search_row};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::index::{self, IndexEntry};
use crate::mirror::{self, MirrorRepo};
use crate::names::{PkgBase, PkgName, PkgTarget, RepoName, RepoRank, SearchTerm};
use crate::pacman::alpm_db::{self, PacmanIndex, SyncInfo};
use crate::pacman::invoke::{self, PkgUpgrade, REPO_AUR};
use crate::pacman::preflight;
use crate::paths;
use crate::resolver::Plan;
use crate::ui::{self, UpgradeSelection};
use crate::version::Version;
use cart::{
    ApplyOutcome, Approval, ApproveResult, AurApproval, Cart, CartItem, KeepResult, ReviewOutcome,
    Source, StageClass, StageResult, UnstageResult,
};
use command::Command;
use complete::ShellHelper;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{ColorMode as RlColorMode, Config as RlConfig, Editor};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, SystemTime};
use tracing::{debug, info, instrument};

pub mod cart;
pub mod command;
pub mod complete;
pub mod selector;
pub mod upgrade;

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
    /// Refresh the mirror + index, reload the session (so `search`/`info` see
    /// fresh data too), and return the current upgrade candidates (repo ∪ AUR)
    /// for `upgrade` to seed into the cart.
    fn upgrade(&mut self) -> Result<Vec<PkgUpgrade>>;
    /// Re-fetch the mirror + index and reload the session (fresh data for
    /// `search`/`info`/classification/completion) **without** seeding the cart —
    /// `upgrade` is the stage-the-upgrades variant; `refresh` is just the
    /// re-fetch.
    fn refresh(&mut self) -> Result<()>;
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
}

/// Pure command dispatch: map a parsed [`Command`] to side effects + control
/// flow.
///
/// Side effects go through `env`/`self`; dispatch does no I/O of its own, so the
/// command surface and exit conditions are testable without a terminal. Each
/// argument-bearing verb is a method on [`State`] below.
impl State {
    pub fn dispatch<E: ShellEnv>(&mut self, cmd: &Command, env: &mut E) -> Flow {
        match cmd {
            Command::Empty => Flow::Continue,
            Command::Quit => Flow::Exit(0),
            Command::Syntax(msg) => {
                env.print(&format!("syntax error: {msg}"));
                Flow::Continue
            }
            Command::Unknown(verb) => {
                env.print(&format!(
                    "unknown command `{verb}` — type `help` for the command list"
                ));
                Flow::Continue
            }
            Command::Help(topic) => {
                match topic {
                    None => env.print(HELP_TEXT),
                    Some(t) => env.print(&help_topic(t)),
                }
                Flow::Continue
            }
            Command::Search(terms) => {
                self.search(terms, env);
                Flow::Continue
            }
            Command::Info(args) => {
                self.info(args, env);
                Flow::Continue
            }
            Command::Upgrade(args) => {
                self.upgrade(args, env);
                Flow::Continue
            }
            Command::Add(args) => {
                self.add(args, env);
                Flow::Continue
            }
            Command::Drop(args) => {
                self.discard(args, env);
                Flow::Continue
            }
            Command::Keep(args) => {
                self.keep(args, env);
                Flow::Continue
            }
            Command::Remove(args) => {
                self.remove(args, env);
                Flow::Continue
            }
            Command::Approve(args) => {
                self.approve(args, env);
                Flow::Continue
            }
            Command::Review(args) => {
                self.review(args, env);
                Flow::Continue
            }
            Command::Show => {
                // `show` brings the transaction to the foreground, so numbers now
                // address its rows.
                self.view = View::Cart;
                self.show(env);
                Flow::Continue
            }
            Command::Apply => {
                self.apply(env);
                Flow::Continue
            }
            Command::Undo => {
                self.undo(env);
                Flow::Continue
            }
            Command::Redo => {
                self.redo(env);
                Flow::Continue
            }
            Command::Clear => {
                if self.cart.is_empty() {
                    env.print("cart is already empty");
                } else {
                    self.push_undo(self.cart.clone());
                    self.cart.clear();
                    env.print("cart cleared — `undo` to restore");
                }
                Flow::Continue
            }
            Command::Refresh => {
                // Re-fetch + reload the session; the cart is left untouched
                // (`upgrade` is the seed-the-cart variant).
                match env.refresh() {
                    Ok(()) => env.print("mirror + index refreshed"),
                    Err(e) => env.print(&format!("refresh: {e}")),
                }
                Flow::Continue
            }
        }
    }

    /// `search <terms…>`: run the query, print a numbered list, remember it.
    fn search<E: ShellEnv>(&mut self, terms: &[SearchTerm], env: &mut E) {
        if terms.is_empty() {
            env.print("usage: search <terms…>");
            return;
        }
        match env.search(terms) {
            Ok(items) => {
                if items.is_empty() {
                    let joined = terms
                        .iter()
                        .map(SearchTerm::as_str)
                        .collect::<Vec<_>>()
                        .join(" ");
                    env.print(&format!("no packages match `{joined}`"));
                } else {
                    // `items` is best-first (row 1 = best). Print it worst-first
                    // so the strongest matches land at the bottom, next to the
                    // prompt the shell scrolls to — and the low, easy-to-type
                    // numbers are the good ones. The numbers still key the
                    // best-first `search_list`, so `add 1` is always the top match
                    // regardless of print direction.
                    for (i, item) in items.iter().enumerate().rev() {
                        env.print(&format!("{:>3}  {}", i + 1, item.label));
                    }
                }
                // Replace the current list even when empty, so a stale list
                // can't be addressed by number after a fruitless search, and
                // make the search results the active numbered view.
                self.search_list = items;
                self.view = View::Search;
            }
            Err(e) => env.print(&format!("search: {e}")),
        }
    }

    /// `info <sel…>`: resolve the selectors and show details. Reads the current
    /// list but doesn't mutate session state.
    fn info<E: ShellEnv>(&self, args: &[String], env: &mut E) {
        if args.is_empty() {
            env.print("usage: info <pkg|number|range|glob>…");
            return;
        }
        let targets = match self.resolve_against_list(args, env) {
            Ok(t) => t,
            Err(e) => {
                env.print(&format!("info: {e}"));
                return;
            }
        };
        if targets.is_empty() {
            env.print("info: nothing matched");
            return;
        }
        if let Err(e) = env.show_info(&targets) {
            env.print(&format!("info: {e}"));
        }
    }

    /// `upgrade [sel…]`: refresh + recompute the available upgrades and seed
    /// them into the cart (repo → approved, AUR → needs-review per config). With
    /// `sel…`, seed only the matching subset (numbers index the freshly computed
    /// list; names/globs match candidate names). Then `show`s the cart.
    fn upgrade<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
        let candidates = match env.upgrade() {
            Ok(c) => c,
            Err(e) => {
                env.print(&format!("upgrade: {e}"));
                return;
            }
        };
        if candidates.is_empty() {
            env.print("nothing to upgrade");
            return;
        }
        let to_seed = if args.is_empty() {
            candidates
        } else {
            match select_from_candidates(args, &candidates) {
                Ok(v) => v,
                Err(e) => {
                    env.print(&format!("upgrade: {e}"));
                    return;
                }
            }
        };
        let policy = env.aur_policy();
        let before = self.cart.clone();
        let mut staged = 0;
        for u in to_seed {
            if self.cart.add(CartItem::from_upgrade(u, policy)) == StageResult::Staged {
                staged += 1;
            }
        }
        if staged > 0 {
            self.push_undo(before);
        }
        // The seeded transaction is now the foreground list.
        self.view = View::Cart;
        env.print(&format!("{staged} upgrade(s) staged"));
        self.show(env);
    }

    /// `add <sel…>`: classify each selected target and stage it. Selectors
    /// resolve against the active list (numbers) + the full name universe
    /// (names/globs), so you can `add` anything installable. `add` reads the
    /// active list but doesn't switch it, so a run of `add`s keeps working
    /// through a search list even though each reprints the cart.
    fn add<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
        if args.is_empty() {
            env.print("usage: add <pkg|number|range|glob>…");
            return;
        }
        let targets = match self.resolve_against_list(args, env) {
            Ok(t) => t,
            Err(e) => {
                env.print(&format!("add: {e}"));
                return;
            }
        };
        if targets.is_empty() {
            env.print("add: nothing matched");
            return;
        }
        let policy = env.aur_policy();
        let before = self.cart.clone();
        let mut changed = false;
        for t in targets {
            match env.classify(&t) {
                Some(StageClass { source, repo }) => {
                    let name = t.as_str().to_owned();
                    // Show the concrete repo (`core`/`extra`) when known, else
                    // the coarse source label.
                    let label = repo
                        .clone()
                        .map_or_else(|| source.label().to_owned(), RepoName::into_inner);
                    match self.cart.add(CartItem::new(t, source, repo, policy)) {
                        StageResult::Staged => {
                            env.print(&format!("staged {name} ({label})"));
                            changed = true;
                        }
                        StageResult::AlreadyStaged => {
                            env.print(&format!("{name} is already staged"));
                        }
                    }
                }
                None => env.print(&format!("unknown package `{}` — not staged", t.as_str())),
            }
        }
        // Keep the resulting transaction on screen so the user needn't `show`
        // after every stage (post-5c UX). Skipped when nothing actually changed
        // (all already-staged / unknown), so a no-op `add` stays quiet.
        if changed {
            self.push_undo(before);
            self.show(env);
        }
    }

    /// `drop <sel…>`: unstage installs from the cart. Names/globs match staged
    /// specs; numbers index the active list (see [`View`]). A `drop` narrows the
    /// cart, so afterwards the cart is the foreground list.
    fn discard<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
        if args.is_empty() {
            env.print("usage: drop <pkg|number|range|glob>…");
            return;
        }
        let targets = match self.resolve_against_cart(args) {
            Ok(t) => t,
            Err(e) => {
                env.print(&format!("drop: {e}"));
                return;
            }
        };
        if targets.is_empty() {
            env.print("drop: nothing in the cart matched");
            return;
        }
        let before = self.cart.clone();
        let mut changed = false;
        for t in targets {
            match self.cart.unstage(&t) {
                UnstageResult::Unstaged => {
                    env.print(&format!("dropped {}", t.as_str()));
                    changed = true;
                }
                UnstageResult::NotStaged => env.print(&format!("{} wasn't staged", t.as_str())),
            }
        }
        // Reprint the remaining transaction (or "cart is empty" once the last row
        // goes) so the current cart is always on screen — post-5c UX.
        if changed {
            self.push_undo(before);
            self.view = View::Cart;
            self.show(env);
        }
    }

    /// `keep <sel…>`: keep only the selected install rows, dropping every other
    /// staged install — the inverse of `drop`, for narrowing a large cart down to
    /// a few packages (`upgrade`, then `keep glibc firefox`). Selectors resolve
    /// against the cart, exactly like `drop`; staged removals are untouched. A
    /// selector matching nothing leaves the cart intact rather than emptying it.
    fn keep<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
        if args.is_empty() {
            env.print("usage: keep <pkg|number|range|glob>… (try `keep aur`)");
            return;
        }
        let targets = match self.resolve_against_cart(args) {
            Ok(t) => t,
            Err(e) => {
                env.print(&format!("keep: {e}"));
                return;
            }
        };
        let keep: HashSet<&str> = targets.iter().map(PkgTarget::as_str).collect();
        let before = self.cart.clone();
        match self.cart.keep(&keep) {
            KeepResult::NoMatch => {
                env.print("keep: nothing in the cart matched — cart unchanged");
            }
            KeepResult::Kept { dropped } if dropped.is_empty() => {
                env.print("keep: every staged package is already kept — nothing dropped");
            }
            KeepResult::Kept { dropped } => {
                for spec in &dropped {
                    env.print(&format!("dropped {}", spec.as_str()));
                }
                self.push_undo(before);
                // Reprint the narrowed cart, matching `drop`'s post-5c UX.
                self.view = View::Cart;
                self.show(env);
            }
        }
    }

    /// `remove <sel…>`: stage an uninstall (`pacman -R` at apply). Selectors
    /// resolve against the active list + universe; pacman validates names at
    /// apply time.
    ///
    /// A selector that lands on a staged *fresh install* is rejected with a
    /// pointer to `drop`: the package isn't installed, so staging a `-R` for
    /// something the transaction is about to install is a contradiction — the
    /// user almost certainly means "take it out of the cart" (`drop`). A staged
    /// *upgrade* row is the opposite case: the package IS installed, so `remove`
    /// wins over the pending upgrade — the row leaves the cart and the removal
    /// is staged in its place.
    fn remove<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
        if args.is_empty() {
            env.print("usage: remove <pkg|number|range|glob>…");
            return;
        }
        let targets = match self.resolve_against_list(args, env) {
            Ok(t) => t,
            Err(e) => {
                env.print(&format!("remove: {e}"));
                return;
            }
        };
        if targets.is_empty() {
            env.print("remove: nothing matched");
            return;
        }
        let before = self.cart.clone();
        let mut changed = false;
        for t in targets {
            // `Some(is_upgrade)` when the target is a staged install row.
            match self.cart.item(&t).map(|i| i.upgrade.is_some()) {
                // A fresh-install row isn't installed — you can't uninstall
                // it. Point at `drop`, which is what "get rid of this cart
                // row" means, and stage nothing.
                Some(false) => {
                    env.print(&format!(
                        "{name} is staged for install, not installed — `drop {name}` to unstage it",
                        name = t.as_str()
                    ));
                    continue;
                }
                // An upgrade row is an installed package: removing it wins
                // over upgrading it, so the row makes way for the removal.
                Some(true) => {
                    self.cart.unstage(&t);
                    changed = true;
                    let name = PkgName::from(t.into_inner());
                    match self.cart.stage_remove(name.clone()) {
                        StageResult::Staged => env.print(&format!(
                            "{name} was staged for upgrade — staged removal instead"
                        )),
                        StageResult::AlreadyStaged => env.print(&format!(
                            "{name}: dropped the staged upgrade; already staged for removal"
                        )),
                    }
                    continue;
                }
                None => {}
            }
            let name = PkgName::from(t.into_inner());
            match self.cart.stage_remove(name.clone()) {
                StageResult::Staged => {
                    env.print(&format!("staged removal of {name}"));
                    changed = true;
                }
                StageResult::AlreadyStaged => {
                    env.print(&format!("{name} is already staged for removal"));
                }
            }
        }
        // Show the resulting transaction (the new "will remove" row included) so
        // the cart is always on screen — post-5c UX.
        if changed {
            self.push_undo(before);
            self.show(env);
        }
    }

    /// `approve <sel…>` / `approve *`: mark staged AUR items approved without
    /// opening a diff. Repo items are already approved; selectors resolve
    /// against the cart (`*` matches every staged item).
    fn approve<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
        if args.is_empty() {
            env.print("usage: approve <pkg|number|range|glob>… (try `approve *`)");
            return;
        }
        let targets = match self.resolve_against_cart(args) {
            Ok(t) => t,
            Err(e) => {
                env.print(&format!("approve: {e}"));
                return;
            }
        };
        if targets.is_empty() {
            env.print("approve: nothing in the cart matched");
            return;
        }
        let before = self.cart.clone();
        let mut changed = false;
        for t in targets {
            match self.cart.approve(&t) {
                ApproveResult::Approved => {
                    if let Some(pb) = env.pkgbase_of(&t) {
                        self.cart.mark_reviewed(pb);
                    }
                    env.print(&format!("approved {}", t.as_str()));
                    changed = true;
                }
                ApproveResult::AlreadyApproved => {
                    env.print(&format!("{} is already approved", t.as_str()));
                }
                ApproveResult::NotStaged => {
                    env.print(&format!("{} isn't staged", t.as_str()));
                }
            }
        }
        if changed {
            self.push_undo(before);
        }
    }

    /// `review [sel…]`: open each selected AUR item's PKGBUILD (diff-against-
    /// installed or full) and approve/skip per the user's call. With no
    /// selector, walk the whole cart — every AUR item still awaiting review —
    /// so `review` alone starts the review loop. Repo items have no PKGBUILD;
    /// already-approved items are left alone; an abort stops the pass.
    fn review<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
        let targets = if args.is_empty() {
            // Collect owned targets so the `self.cart` borrow from
            // `pending_review` is released before the loop mutates it.
            let pending: Vec<PkgTarget> = self
                .cart
                .pending_review()
                .iter()
                .map(|i| PkgTarget::new(i.spec()))
                .collect();
            if pending.is_empty() {
                env.print("nothing to review — all staged packages are approved");
                return;
            }
            pending
        } else {
            match self.resolve_against_cart(args) {
                Ok(t) => t,
                Err(e) => {
                    env.print(&format!("review: {e}"));
                    return;
                }
            }
        };
        if targets.is_empty() {
            env.print("review: nothing in the cart matched");
            return;
        }
        let before = self.cart.clone();
        let mut approved_any = false;
        // Flips to `Auto` once the user picks "approve all": the remaining AUR
        // items clear without opening another diff.
        let mut prompting = review::Prompting::default();
        for t in targets {
            // Copy out (source, approval) so the cart isn't borrowed across the
            // `env.review` call (which then mutates the cart on approval).
            match self.cart.item(&t).map(|i| (i.source, i.approval)) {
                None => {}
                Some((Source::Repo, _)) => {
                    env.print(&format!(
                        "{} is a repo package — nothing to review",
                        t.as_str()
                    ));
                }
                Some((_, Approval::Approved)) => {
                    env.print(&format!("{} is already approved", t.as_str()));
                }
                Some((Source::Aur, Approval::NeedsReview)) => {
                    if prompting == review::Prompting::Auto {
                        // "approve all" was chosen earlier — no more diffs.
                        self.approve_reviewed(&t, env);
                        approved_any = true;
                        continue;
                    }
                    match env.review(&t) {
                        Ok(ReviewOutcome::Approved) => {
                            self.approve_reviewed(&t, env);
                            approved_any = true;
                        }
                        Ok(ReviewOutcome::ApprovedAll) => {
                            self.approve_reviewed(&t, env);
                            approved_any = true;
                            prompting = review::Prompting::Auto;
                        }
                        Ok(ReviewOutcome::Skipped) => {
                            env.print(&format!("{} left for review", t.as_str()));
                        }
                        Ok(ReviewOutcome::Aborted) => {
                            env.print("review aborted");
                            break;
                        }
                        Err(e) => {
                            env.print(&format!("review {}: {e}", t.as_str()));
                            break;
                        }
                    }
                }
            }
        }
        if approved_any {
            self.push_undo(before);
        }
    }

    /// Clear a just-reviewed AUR target's approval gate: approve it, record its
    /// pkgbase in the reviewed set (so the build pipeline won't re-prompt), and
    /// acknowledge it. Shared by the per-item `review` approval and the
    /// "approve all" fast path.
    fn approve_reviewed<E: ShellEnv>(&mut self, t: &PkgTarget, env: &mut E) {
        self.cart.approve(t);
        if let Some(pb) = env.pkgbase_of(t) {
            self.cart.mark_reviewed(pb);
        }
        env.print(&format!("approved {}", t.as_str()));
    }

    /// `show`: render the staged transaction — a header, the install/removal
    /// table (delegated to the env for color + alignment + age), and whether
    /// `apply` is ready.
    ///
    /// The header and the approval summary are deterministic and stay here in
    /// the pure core (so they're unit-testable via the fake env); the table body
    /// — color, column widths, per-AUR-row age — is I/O-shaped presentation and
    /// goes through [`ShellEnv::render_cart`].
    fn show<E: ShellEnv>(&self, env: &mut E) {
        let cart = &self.cart;
        if cart.is_empty() {
            env.print("cart is empty — `add <pkg>` to stage an install");
            return;
        }
        env.print(&format!(
            "transaction — {} to install, {} to remove",
            cart.items().len(),
            cart.removals().len()
        ));
        env.render_cart(cart);
        let pending = cart.pending_review().len();
        if pending == 0 {
            env.print("all approved — run `apply`");
        } else {
            env.print(&format!(
                "{pending} package(s) need review — run `review <sel>` or `approve <sel>`"
            ));
        }
    }

    /// `apply`: gate on every staged item being approved, then run the
    /// transaction. A clean run clears the applied rows; a declined one keeps
    /// the cart; a failed one keeps it intact so the user can `drop` the
    /// offender and retry.
    fn apply<E: ShellEnv>(&mut self, env: &mut E) {
        if self.cart.is_empty() {
            env.print("cart is empty — nothing to apply");
            return;
        }
        let pending = self.cart.pending_review();
        if !pending.is_empty() {
            let names: Vec<&str> = pending.iter().map(|i| i.spec()).collect();
            env.print(&format!(
                "needs review: {} — run `review <sel>` or `approve <sel>`",
                names.join(", ")
            ));
            return;
        }
        match env.apply(&self.cart) {
            Ok(ApplyOutcome::Declined) => env.print("apply cancelled — cart kept"),
            Ok(ApplyOutcome::Succeeded) => {
                self.cart.clear_applied();
                // The transaction ran: the cart is a new epoch, so pre-apply
                // undo snapshots (which would re-stage now-installed packages)
                // no longer make sense.
                self.clear_undo_history();
                env.print("done");
            }
            Ok(ApplyOutcome::Failed { installed }) => {
                // Drop the rows that actually landed so a retry doesn't reinstall
                // them; keep the offenders (and any staged removals, which don't
                // run once a build fails) staged for `drop`/fix + `apply` again.
                let landed = installed.len();
                for t in &installed {
                    self.cart.unstage(t);
                }
                // A run happened — old undo snapshots reference a pre-apply world.
                self.clear_undo_history();
                if landed == 0 {
                    env.print("apply failed — nothing installed; cart kept for retry");
                } else {
                    env.print(&format!(
                        "apply partly failed — {landed} installed (dropped), \
                         {} still staged; fix and `apply` again",
                        self.cart.items().len()
                    ));
                }
                // Reprint what's left so the failures are on screen to act on.
                self.view = View::Cart;
                self.show(env);
            }
            Err(e) => env.print(&format!("apply: {e}")),
        }
    }

    /// Forget the `undo`/`redo` stacks — after a transaction runs, the snapshots
    /// describe a world that no longer exists.
    fn clear_undo_history(&mut self) {
        self.history.clear();
        self.redo.clear();
    }

    /// Resolve selector `args` for a cart verb (`drop`, `keep`, `approve`,
    /// `review`): a repo name (`aur`, `core`, …) selects every staged row from
    /// that repo, and names/globs match staged specs — both scoped to what's
    /// staged, since a cart verb acts on the cart regardless of which list is up.
    /// Numbers, though, index the *active* list (see [`View`]), so a bare `3`
    /// means the same row it would for any other verb — the one you last saw.
    fn resolve_against_cart(&self, args: &[String]) -> std::result::Result<Vec<PkgTarget>, String> {
        let rows: Vec<RepoRow> = self
            .cart
            .items()
            .iter()
            .map(|it| RepoRow {
                target: PkgTarget::new(it.spec()),
                repo: Some(it.repo_label()),
            })
            .collect();
        let args = expand_repo_tokens(args, &rows);
        let universe: Vec<PkgTarget> = rows.iter().map(|r| r.target.clone()).collect();
        selector::resolve(&args, &self.active_list(), &universe)
    }

    /// Resolve selector `args` for a list verb (`add`, `info`, `remove`): a repo
    /// name selects every row from that repo in the active list, numbers/ranges
    /// index the active list, and names/globs resolve against the full name
    /// universe (so you can `add` anything installable, not just what's shown).
    fn resolve_against_list<E: ShellEnv>(
        &self,
        args: &[String],
        env: &E,
    ) -> std::result::Result<Vec<PkgTarget>, String> {
        let active = self.active_list();
        let rows: Vec<RepoRow> = active
            .iter()
            .map(|it| RepoRow {
                target: it.target.clone(),
                repo: it.repo.clone(),
            })
            .collect();
        let args = expand_repo_tokens(args, &rows);
        selector::resolve(&args, &active, env.names())
    }

    /// The staged cart as a numbered list — the same rows, in the same order,
    /// that `show` prints — so a number resolves to the row the user sees. Built
    /// live from the cart, so it can never lag a staging change.
    fn cart_as_list(&self) -> Vec<ListItem> {
        self.cart
            .items()
            .iter()
            .map(|it| ListItem {
                target: PkgTarget::new(it.spec()),
                label: String::new(),
                repo: Some(it.repo_label()),
            })
            .collect()
    }

    /// The list bare numbers currently index: the search results while the search
    /// view is up, else the staged cart (see [`View`]).
    ///
    /// The search view falls back to the cart when there's no search list to
    /// address — a fresh session (never searched) or a fruitless search — so a
    /// number always resolves against whatever numbered table is actually on
    /// screen, which after an `add`/`drop`/… is the cart.
    fn active_list(&self) -> Vec<ListItem> {
        match self.view {
            View::Search if !self.search_list.is_empty() => self.search_list.clone(),
            _ => self.cart_as_list(),
        }
    }

    /// Snapshot the pre-change cart onto the `undo` stack (bounded) and discard
    /// any redo branch. Call with the cart as it was *before* a cart-changing
    /// command mutates it — only when the command actually changed something, so
    /// a no-op never consumes an undo step.
    fn push_undo(&mut self, before: Cart) {
        self.history.push(before);
        if self.history.len() > UNDO_DEPTH {
            self.history.remove(0);
        }
        self.redo.clear();
    }

    /// `undo`: revert the last cart-changing command, restoring the cart to how
    /// it was before it ran. The reverted-from cart moves onto the redo stack.
    fn undo<E: ShellEnv>(&mut self, env: &mut E) {
        match self.history.pop() {
            Some(prev) => {
                self.redo.push(std::mem::replace(&mut self.cart, prev));
                self.view = View::Cart;
                env.print("undone — `redo` to reapply");
                self.show(env);
            }
            None => env.print("nothing to undo"),
        }
    }

    /// `redo`: reapply the most recently undone change. The inverse of `undo`;
    /// available only until a fresh cart-changing command clears the redo branch.
    fn redo<E: ShellEnv>(&mut self, env: &mut E) {
        match self.redo.pop() {
            Some(next) => {
                self.history.push(std::mem::replace(&mut self.cart, next));
                self.view = View::Cart;
                env.print("redone");
                self.show(env);
            }
            None => env.print("nothing to redo"),
        }
    }
}

/// One `(target, repo)` pair fed to [`expand_repo_tokens`] — the minimal view of
/// a cart row or list row a repo-name selector needs.
struct RepoRow {
    target: PkgTarget,
    repo: Option<RepoName>,
}

/// Rewrite repo-name tokens (`aur`, `core`, `extra`, …) into the targets of the
/// rows whose repo matches, so `drop aur` / `add extra` act on a whole repo.
///
/// A token that matches no row's repo is passed through unchanged for the
/// number/range/name/glob selector to handle — so a real package that happens
/// to share a repo's name still resolves normally when nothing in the current
/// scope is from that repo. Matching is case-insensitive. The expansion emits
/// selector tokens (the matched targets' names) so the one resolution path in
/// [`selector::resolve`] still does the indexing, dedup, and ordering.
fn expand_repo_tokens(args: &[String], rows: &[RepoRow]) -> Vec<String> {
    args.iter()
        .flat_map(|a| {
            let matched: Vec<String> = rows
                .iter()
                .filter(|r| {
                    r.repo
                        .as_ref()
                        .is_some_and(|repo| repo.as_str().eq_ignore_ascii_case(a))
                })
                .map(|r| r.target.as_str().to_owned())
                .collect();
            if matched.is_empty() {
                vec![a.clone()]
            } else {
                matched
            }
        })
        .collect()
}

/// Filter `candidates` to those a selector matches: a repo name (`aur`, `core`,
/// …) selects every candidate from that repo; numbers index the candidate list;
/// names/globs match candidate names. Reuses the selector core (the same one
/// `add`/`info`/cart verbs use), so `upgrade glibc python-*` and `upgrade aur`
/// work the same.
fn select_from_candidates(
    args: &[String],
    candidates: &[PkgUpgrade],
) -> std::result::Result<Vec<PkgUpgrade>, String> {
    let rows: Vec<RepoRow> = candidates
        .iter()
        .map(|u| RepoRow {
            target: PkgTarget::new(u.name.as_str()),
            repo: Some(u.repo.clone()),
        })
        .collect();
    let args = expand_repo_tokens(args, &rows);
    let list: Vec<ListItem> = rows
        .iter()
        .map(|r| ListItem {
            target: r.target.clone(),
            label: String::new(),
            repo: r.repo.clone(),
        })
        .collect();
    let universe: Vec<PkgTarget> = rows.iter().map(|r| r.target.clone()).collect();
    let picked = selector::resolve(&args, &list, &universe)?;
    let names: HashSet<&str> = picked.iter().map(PkgTarget::as_str).collect();
    Ok(candidates
        .iter()
        .filter(|u| names.contains(u.name.as_str()))
        .cloned()
        .collect())
}

/// The `help` command body. A flat command list; per-command topics land with
/// the commands themselves.
const HELP_TEXT: &str = "\
commands:
  search <terms…>     find packages (repo + AUR)
  info <sel…>         show package details (sel = name | number | range | glob)
  add <sel…>          stage packages to install
  drop <sel…>         unstage packages from the cart (alias: discard)
  keep <sel…>         keep only these staged packages, drop the rest
  remove <sel…>       stage packages to uninstall
  upgrade [pkg…]      upgrade installed packages (repo + AUR)
  review [sel…]       view a PKGBUILD/diff and approve it (no sel = review all)
  approve <sel…>      approve staged AUR packages without a diff (try `approve *`)
  show                preview the staged transaction
  apply               build + install the staged transaction
  undo                revert the last cart change
  redo                reapply the last undone change
  clear               empty the cart
  refresh             re-fetch the AUR mirror + index
  help [topic]        this list, or `help <command>` for detail on one
  quit                leave the shell (also: Ctrl-D)
selectors: `3` (row), `5-8` (range), `glibc` (name), `python-*` (glob),
           `aur`/`core`/… (whole repo — e.g. `drop aur`, `add extra`)
numbers index the list you last brought up — search results (`search`) or the
transaction (`show`/`upgrade`/`drop`/`keep`)";

/// Per-command help shown by `help <topic>`, keyed by canonical verb (the same
/// order as [`command::VERBS`]). Each body opens with a usage line (and any
/// aliases) then a short paragraph — enough to answer "what does this verb do
/// and what does it act on" without leaving the shell.
const TOPICS: &[(&str, &str)] = &[
    (
        "search",
        "search <terms…>\n  \
         Query repos + AUR by name, description, and provides. Prints a numbered,\n  \
         ranked list (best matches nearest the prompt) and remembers it, so a later\n  \
         `add 3` / `info 1-4` can index it by number.",
    ),
    (
        "info",
        "info <sel…>\n  \
         Show package details. sel = name, number (a row in the shown list), range\n  \
         (`5-8`), or glob (`python-*`).",
    ),
    (
        "add",
        "add <sel…>   (alias: install)\n  \
         Stage packages to install in the pending transaction. Resolves against the\n  \
         last list, the AUR index, and the sync DBs — you can add anything.",
    ),
    (
        "drop",
        "drop <sel…>   (aliases: discard, unstage)\n  \
         Un-stage packages from the cart — resolves against what's staged. `drop aur`\n  \
         un-stages every AUR row. Distinct from `remove`, which stages an uninstall.",
    ),
    (
        "keep",
        "keep <sel…>   (alias: only)\n  \
         Keep only the selected staged packages and drop the rest — the inverse of\n  \
         `drop`.",
    ),
    (
        "remove",
        "remove <sel…>   (aliases: uninstall, rm)\n  \
         Stage an uninstall (`pacman -R`) in the transaction. Note the difference from\n  \
         `drop`: `drop` un-stages a pending install, `remove` stages a removal.",
    ),
    (
        "upgrade",
        "upgrade [sel…]   (alias: up)\n  \
         Refresh, recompute the available upgrades, and stage them (repo → approved,\n  \
         AUR → needs review). With sel…, stage only the matching subset.",
    ),
    (
        "review",
        "review [sel…]\n  \
         Open a PKGBUILD/diff for staged AUR packages and approve / skip / discard\n  \
         each. No sel reviews every AUR item still awaiting review.",
    ),
    (
        "approve",
        "approve <sel…>\n  \
         Approve staged AUR packages without opening a diff. `approve *` approves\n  \
         every staged AUR package at once.",
    ),
    (
        "show",
        "show   (aliases: status, ls)\n  \
         Preview the staged transaction: the change-set table with download sizes,\n  \
         build time, and totals.",
    ),
    (
        "apply",
        "apply   (aliases: commit, do)\n  \
         Build + install the staged transaction in one sudo batch. Runs only when\n  \
         every staged package is approved; an interrupted or failed apply drops back\n  \
         to the shell with the cart intact so you can `drop` the offender and retry.",
    ),
    (
        "undo",
        "undo\n  \
         Revert the last cart-changing command (add / drop / keep / remove /\n  \
         upgrade / approve / clear) — e.g. undo a `keep` that dropped too much.\n  \
         Steps back through the session's edits; `redo` reapplies. A run\n  \
         (`apply`) forgets the history.",
    ),
    (
        "redo",
        "redo\n  \
         Reapply the change `undo` just reverted. Available until the next\n  \
         cart-changing command, which forks a new edit branch.",
    ),
    ("clear", "clear\n  Empty the cart."),
    (
        "refresh",
        "refresh\n  \
         Re-fetch the AUR mirror and reload the index — fresh data for\n  \
         search / info / upgrade / completion. Leaves the cart untouched.",
    ),
    (
        "help",
        "help [topic]\n  \
         List the commands, or `help <command>` for detail on one.",
    ),
    (
        "quit",
        "quit   (aliases: exit, q; also Ctrl-D)\n  Leave the shell.",
    ),
];

/// Detailed help for one `help <topic>` argument. Canonicalizes `topic` through
/// [`command::parse`] so aliases (`discard`, `up`, `ls`, …) resolve to their verb
/// for free, then looks it up in [`TOPICS`]. An unrecognized topic points back at
/// the bare `help` list rather than erroring.
fn help_topic(topic: &str) -> String {
    let verb = command::parse(topic).verb();
    TOPICS.iter().find(|(v, _)| *v == verb).map_or_else(
        || format!("no help for `{topic}` — type `help` for the command list"),
        |(_, body)| (*body).to_owned(),
    )
}

/// Run the interactive shell. Returns the desired process exit code.
///
/// `initial_search` seeds the session: when launched via the bare-positional
/// shortcut (`aurox <term>…`), dispatch passes the typed terms here and the shell
/// runs one `search` before the prompt loop — identical to starting the shell
/// and typing `search <term>…`. Empty for the plain no-arg `aurox` launch.
#[instrument(skip(cfg))]
pub fn run(cfg: &Config, devel: DevelPolicy, initial_search: &[SearchTerm]) -> Result<u8> {
    info!(devel = ?devel, terms = initial_search.len(), "shell session start");
    // Once per session: load the AUR index (+ secondary maps) and the name
    // universe. Not repeated per command; `refresh` (later phase) re-fetches.
    let session = UpgradeSession::load(cfg)?;
    let caches = build_universe(session.as_ref());
    debug!(
        names = caches.universe.len(),
        sync = caches.sync.len(),
        has_index = session.is_some(),
        "shell session loaded"
    );
    let mut env = RealEnv {
        cfg,
        devel,
        session,
        caches,
        view: None,
    };
    let mut state = State::default();

    env.print("aurox shell — type `help` for commands, `quit` to leave");
    if env.session.is_none() {
        env.print("no AUR index yet — run `aurox -Sy` to enable AUR search/info");
    }

    // Seed the session with the launch-time search (`aurox <term>…`): run it once
    // up front so the numbered result list is on screen before the first prompt,
    // exactly as if the user had typed `search <term>…`.
    if !initial_search.is_empty() {
        state.dispatch(&Command::Search(initial_search.to_vec()), &mut env);
    }

    let helper = ShellHelper::new(Rc::clone(&env.caches.universe));
    // Follow the session's colour mode so `--color never` also stops rustyline
    // from dimming the history hint (it skips `highlight_hint` when Disabled).
    let rl_config = RlConfig::builder()
        .color_mode(match cfg.color_mode() {
            ui::ColorMode::Always => RlColorMode::Forced,
            ui::ColorMode::Never => RlColorMode::Disabled,
            ui::ColorMode::Auto => RlColorMode::Enabled,
        })
        .build();
    let mut rl: Editor<ShellHelper, DefaultHistory> = Editor::with_config(rl_config)
        .map_err(|e| Error::other(format!("shell: init line editor: {e}")))?;
    rl.set_helper(Some(helper));
    let history = paths::shell_history_path();
    // A missing history file on first run is expected, not an error.
    rl.load_history(&history).ok();

    let code = loop {
        match rl.readline("aurox> ") {
            Ok(line) => {
                if !line.trim().is_empty() {
                    // Best-effort: a full history ring shouldn't abort input.
                    rl.add_history_entry(line.as_str()).ok();
                }
                let flow = state.dispatch(&command::parse(&line), &mut env);
                // Refresh Tab's view for the next line: the just-mutated cart,
                // and the universe (a cheap `Rc` clone — only `upgrade`/`refresh`
                // actually swaps it). Sharing the same sources the selector
                // resolver uses keeps "what Tab offers" == "what the verb accepts".
                if let Some(helper) = rl.helper_mut() {
                    helper.sync(Rc::clone(&env.caches.universe), cart_targets(&state));
                }
                if let Flow::Exit(code) = flow {
                    break code;
                }
            }
            // Ctrl-C cancels the current line; it does NOT leave the shell.
            Err(ReadlineError::Interrupted) => {}
            // Ctrl-D at the prompt exits cleanly.
            Err(ReadlineError::Eof) => break 0,
            Err(e) => return Err(Error::other(format!("shell: read line: {e}"))),
        }
    };

    // History persistence is best-effort: a save failure shouldn't fail the run.
    if let Err(e) = rl.save_history(&history) {
        debug!(error = %e, "shell: could not save history");
    }
    Ok(code)
}

/// The per-session name caches, built once at startup in a single alpm pass.
struct NameCaches {
    /// Sorted, de-duplicated — every AUR pkgname + pkgbase from the index plus
    /// sync-repo names, each as a [`PkgTarget`] (the universe a user can name).
    /// Backs glob resolution and tab-completion. An `Rc<[_]>` so the rustyline
    /// completer shares it without copying ~100k names, and a re-`build` on
    /// `upgrade`/`refresh` just swaps the pointer.
    universe: Rc<[PkgTarget]>,
    /// Sync-repo pkgname → its repo (`core`, `extra`, …), for `add`'s coarse
    /// repo/AUR classification and the concrete repo column. The first sync DB
    /// (pacman.conf order) that declares a name wins, matching what pacman would
    /// pull.
    sync: HashMap<PkgName, RepoName>,
}

/// Build the [`NameCaches`] for a session. A missing index or unreadable alpm
/// just yields smaller caches, never an error.
fn build_universe(session: Option<&UpgradeSession>) -> NameCaches {
    let mut universe: Vec<PkgTarget> = Vec::new();
    if let Some(s) = session {
        let by = s.secondary();
        universe.extend(by.by_name.keys().map(PkgTarget::from));
        universe.extend(by.by_pkgbase.keys().map(PkgTarget::from));
    }
    let mut sync = HashMap::new();
    if let Ok(alpm) = alpm_db::open() {
        for db in alpm.syncdbs() {
            for pkg in db.pkgs() {
                universe.push(PkgTarget::new(pkg.name()));
                // First DB to declare the name wins (pacman.conf precedence),
                // so `core` shadows a later `testing` carrying the same pkg.
                sync.entry(PkgName::new(pkg.name()))
                    .or_insert_with(|| RepoName::from(db.name()));
            }
        }
    }
    universe.sort_unstable();
    universe.dedup();
    NameCaches {
        universe: Rc::from(universe.into_boxed_slice()),
        sync,
    }
}

/// The staged specs as the typed [`PkgTarget`]s the completer offers for cart
/// verbs (`drop`/`review`/`approve`). Recomputed after each command since the
/// cart is tiny.
fn cart_targets(state: &State) -> Vec<PkgTarget> {
    state
        .cart
        .items()
        .iter()
        .map(|it| PkgTarget::new(it.spec()))
        .collect()
}

/// Production [`ShellEnv`]: the loaded session + stdout, bridging `upgrade` to
/// the existing loop.
struct RealEnv<'a> {
    cfg: &'a Config,
    devel: DevelPolicy,
    session: Option<UpgradeSession>,
    caches: NameCaches,
    /// Cached resolution of the cart's package set for `show` — see
    /// [`CachedTxn`]. `None` until the first render, after a reload, or after an
    /// `apply` (which may have changed the installed set).
    view: Option<CachedTxn>,
}

/// The expensive, package-set-dependent half of the `show` transaction view:
/// the synced-db size snapshot, the pulled-in dependency rows, and the
/// build-time overlay. Built by [`RealEnv::resolve_view`] and cached so repeated
/// `show`s and the post-mutation cart reprint don't redo the resolver + the two
/// alpm opens + the metrics-store read.
///
/// The resolved [`Plan`] itself isn't kept — the render only needs the dep rows
/// and overlay derived from it, and `apply` resolves its own live plan against
/// the system db. The approval-bearing root rows aren't stored either: they're
/// re-derived per render from the live cart, so `approve`/`review` show up on
/// the next `show` without a re-resolve (only the approval cell changed, not the
/// resolution).
struct ResolvedTxn {
    size_pac: PacmanIndex,
    repo_deps: Vec<PkgName>,
    aur_deps: Vec<PkgBase>,
    metrics: ui::PreviewMetrics,
    /// Sysupgrade preflight notes for the staged repo-upgrade lane (empty when
    /// none is staged or the check couldn't run) — rendered under the table so
    /// "this upgrade will break X" shows on the screen the user curates the
    /// cart from, not first at `apply`.
    preflight: Vec<upgrade::PreflightNote>,
}

/// A [`ResolvedTxn`] tagged with the cart package set it was resolved for, so
/// [`RealEnv::render_cart`] reuses it while that set is unchanged and discards it
/// the moment `add`/`drop`/`remove`/`clear` (or a reload) moves the set.
struct CachedTxn {
    key: TxnKey,
    resolved: ResolvedTxn,
}

/// Identity of a cart's *resolution-relevant* state: the staged install targets
/// plus the removal names. Approval is excluded — it doesn't change what
/// resolves, only the rendered cell — so `approve`/`review` are a cache hit. Two
/// carts with equal keys resolve identically against unchanged mirror/db data,
/// which is why [`RealEnv::reload`] also clears the cache when that data may have
/// moved.
#[derive(PartialEq, Eq)]
struct TxnKey {
    installs: Vec<PkgTarget>,
    removals: Vec<PkgName>,
}

impl TxnKey {
    fn of(cart: &Cart) -> Self {
        // The cart keeps `items` sorted (phase 5b), but normalise defensively so
        // the key is order-independent however it was assembled.
        let mut installs: Vec<PkgTarget> = cart
            .items()
            .iter()
            .map(|it| PkgTarget::new(it.spec()))
            .collect();
        installs.sort_unstable();
        let mut removals: Vec<PkgName> = cart.removals().to_vec();
        removals.sort_unstable();
        Self { installs, removals }
    }
}

impl ShellEnv for RealEnv<'_> {
    fn print(&mut self, line: &str) {
        println!("{line}");
    }

    fn upgrade(&mut self) -> Result<Vec<PkgUpgrade>> {
        // `upgrade` defers to the refresh TTL — a fetch within
        // `refresh_max_age_secs` is skipped (the session still reloads), so
        // back-to-back `upgrade`s don't each pay a network round-trip. The
        // explicit `refresh` command forces a fetch.
        self.reload(upgrade::FetchPolicy::WhenStale)?;
        match &self.session {
            Some(session) => session.recompute_remaining(self.devel),
            // No AUR index even after a refresh: repo upgrades are still
            // queryable straight from the synced db.
            None => invoke::query_repo_upgrades(),
        }
    }

    fn refresh(&mut self) -> Result<()> {
        // `refresh` is the always-fetch command — it ignores the TTL.
        self.reload(upgrade::FetchPolicy::Always)
    }

    fn search(&mut self, terms: &[SearchTerm]) -> Result<Vec<ListItem>> {
        let regexes: Vec<regex::Regex> = terms
            .iter()
            .map(SearchTerm::compile)
            .collect::<std::result::Result<_, _>>()?;
        let mut rows: Vec<Row<'_>> = alpm_db::search_sync(terms)?
            .into_iter()
            .map(Row::Repo)
            .collect();
        if let Some(session) = &self.session {
            let aur = session.secondary().search(session.index(), &regexes);
            rows.extend(aur.into_iter().map(Row::Aur));
        }
        // Rank the merged repo+AUR list best-first (name-prefix > substring >
        // description; shorter names win; AUR ties break freshest-first).
        // `State::search` prints this reversed, so row 1 — the best match — lands
        // right above the prompt.
        rank_rows(&mut rows, &regexes);

        // Resolve installed state + versions against the live pacman DBs and
        // render the aligned table (installed rows emphasized, with an `old → new`
        // diff + build-time estimate). The build-time overlay is filled only for
        // the installed AUR rows.
        let pac = upgrade::system_pac()?;
        let search_rows: Vec<ui::SearchRow> = rows.iter().map(|r| search_row(r, &pac)).collect();
        let metrics = self.search_metrics(&search_rows);
        let table = ui::search_table(&search_rows, &pac, &metrics);
        Ok(table
            .lines()
            .iter()
            .zip(&rows)
            .map(|(line, r)| ListItem {
                target: r.picked(),
                label: line.clone(),
                repo: Some(RepoName::from(r.repo_name())),
            })
            .collect())
    }

    fn show_info(&mut self, targets: &[PkgTarget]) -> Result<()> {
        // Repo wins ties — the same rule as `classify`: pacman owns a name
        // that lives in both a sync repo and the AUR (`info cef` must describe
        // extra/cef, not the same-named AUR pkgbase). Targets are handled in
        // the order given so multi-target output matches pacman's.
        let mut alpm = None;
        // Lazy like `alpm`: opened at the first AUR target, reused for the
        // rest of the command (mirror + localdb handles behind the AUR
        // block's maintainer / first-submitted / installed-size fields).
        let mut aur_sources = None;
        let mut missing: Vec<&PkgTarget> = Vec::new();
        for t in targets {
            if self.caches.sync.contains_key(t.bare()) {
                // One handle for the whole command, opened at the first repo
                // target — repo info works even without an AUR index.
                if alpm.is_none() {
                    alpm = Some(alpm_db::open()?);
                }
                if let Some(info) = alpm.as_ref().and_then(|a| SyncInfo::lookup(a, t.bare())) {
                    info.print();
                    continue;
                }
                // The startup cache said repo but the live DBs disagree (a
                // `pacman -Sy` ran since) — fall through to the AUR lookup.
            }
            if let Some(session) = &self.session
                && index::print_aur_info(
                    session.index(),
                    session.secondary(),
                    t,
                    aur_sources.get_or_insert_with(index::info::InfoSources::open),
                )
            {
                continue;
            }
            missing.push(t);
        }
        if !missing.is_empty() {
            if self.session.is_none() {
                ui::warn("no AUR index; run `aurox -Sy` first");
            }
            ui::warn(&format!(
                "not in repos or AUR: {}",
                missing
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(())
    }

    fn names(&self) -> &[PkgTarget] {
        &self.caches.universe
    }

    fn classify(&self, target: &PkgTarget) -> Option<StageClass> {
        // Repo wins ties: pacman owns sync packages, and the resolver routes a
        // shared name to the repo lane anyway, so auto-approving it is honest.
        if let Some(repo) = self.caches.sync.get(target.bare()) {
            return Some(StageClass {
                source: Source::Repo,
                repo: Some(repo.clone()),
            });
        }
        let session = self.session.as_ref()?;
        session
            .secondary()
            .lookup(session.index(), target.as_str())
            .map(|_| StageClass {
                source: Source::Aur,
                repo: None,
            })
    }

    fn aur_policy(&self) -> AurApproval {
        // The `aur_approval` knob wins when set; unset defers to the legacy
        // `review_default == "skip"` behaviour. Resolution + fallback live on
        // the type so they're unit-tested next to it.
        AurApproval::from_config(self.cfg.aur_approval, &self.cfg.review_default)
    }

    fn pkgbase_of(&self, target: &PkgTarget) -> Option<PkgBase> {
        let session = self.session.as_ref()?;
        session
            .secondary()
            .lookup(session.index(), target.as_str())
            .map(|e| e.pkgbase.clone())
    }

    fn review(&mut self, target: &PkgTarget) -> Result<ReviewOutcome> {
        let Some(session) = &self.session else {
            ui::warn("no AUR index; nothing to review");
            return Ok(ReviewOutcome::Skipped);
        };
        let Some(entry) = session.secondary().lookup(session.index(), target.as_str()) else {
            ui::warn(&format!("{}: not an AUR package", target.as_str()));
            return Ok(ReviewOutcome::Skipped);
        };
        let pkgbase = entry.pkgbase.clone();
        let new_ver = entry.version();

        // Materialise the worktree + resolve the installed counterpart exactly
        // like `build::prepare_one`, so the diff base and review header match
        // what `apply` would show. Fresh `add` targets are unhinted.
        let mirror = MirrorRepo::open(&paths::aur_repo_path())?;
        let wt = mirror::worktree::add_or_reset(&mirror, &pkgbase, &paths::pkg_worktree(&pkgbase))?;
        let alpm = alpm_db::open()?;
        let pac = PacmanIndex::build(&alpm);
        let counterpart = pac.counterpart_with_hint(entry, None);

        match review::review(
            &mirror,
            &pkgbase,
            &new_ver,
            counterpart.as_ref(),
            &wt,
            self.cfg.review_history_scan_max,
            // The shell drives one interactive review per call; the
            // "approve all" fast path is the dispatch loop's job (it decides
            // whether to call this at all), so a single review always prompts.
            review::Prompting::Prompt,
        ) {
            Ok(review::Outcome::Approved) => Ok(ReviewOutcome::Approved),
            Ok(review::Outcome::ApprovedAll) => Ok(ReviewOutcome::ApprovedAll),
            Ok(review::Outcome::Skipped) => Ok(ReviewOutcome::Skipped),
            // An abort at the review prompt ends the pass but not the shell.
            Err(Error::UserAbort) => Ok(ReviewOutcome::Aborted),
            Err(e) => Err(e),
        }
    }

    fn render_cart(&mut self, cart: &Cart) {
        // Render the unified change-set table from the cached resolution (roots
        // + pulled-in deps + removals + cost), re-resolving only when the cart's
        // package set moved. `show` must never error out, so a resolve failure
        // degrades to the flat staged rows plus a note (UPDATE_LOOP goal #5
        // landing behind `show`).
        match self.transaction_view(cart) {
            Ok(table) => {
                for line in table.lines() {
                    self.print(line);
                }
                // The sysupgrade preflight verdict for the staged repo lane —
                // "upgrading X breaks Y" plus the shell-native way out —
                // belongs on this same screen, ahead of any `apply`.
                if let Some(view) = &self.view {
                    for note in &view.resolved.preflight {
                        upgrade::print_preflight_note(note);
                    }
                }
            }
            Err(e) => {
                debug!(error = %e, "show preview resolve failed; flat fallback");
                for line in flat_cart_lines(cart, &e) {
                    self.print(&line);
                }
            }
        }
    }

    fn apply(&mut self, cart: &Cart) -> Result<ApplyOutcome> {
        // The build/install (and any removals) may change the installed set, so
        // the cached resolution is stale once apply runs whatever its outcome;
        // drop it so the next `show` re-resolves against the new system state.
        self.view = None;
        let Some(session) = self.session.as_ref() else {
            ui::warn("no AUR index loaded; cannot apply");
            return Ok(ApplyOutcome::Failed {
                installed: Vec::new(),
            });
        };
        let pac = upgrade::system_pac()?;

        // Preflight the repo-upgrade lane before anything is asked of the user:
        // re-sync the rootless db (the drift guard — the check should see what
        // `pacman -Syu`'s own `-Sy` is about to fetch), recompute the
        // partial-upgrade selection against it, and simulate the `-Su`. Failing
        // that check ends the apply here — before the cost summary, the
        // transaction confirm, and the sudo prompt.
        if !cart.repo_upgrades().is_empty() {
            upgrade::resync_repo_dbs(self.cfg);
        }
        let repo_sel = self.repo_upgrade_selection(session, cart)?;
        let blockers = if repo_sel.repo.is_empty() {
            Vec::new()
        } else {
            match sysupgrade_gate(session, cart, &repo_sel)? {
                Some(blockers) => blockers,
                None => return Ok(ApplyOutcome::Declined),
            }
        };

        // Resolve the build/install half (AUR + fresh installs); repo *upgrades*
        // take the partial `-Syu` lane below, so they're excluded from the plan.
        // Sysupgrade blockers — staged rebuilds whose install is what unblocks
        // that lane — resolve as their own plan so they can run first.
        let (blocker_targets, main_targets): (Vec<build::Target>, Vec<build::Target>) = cart
            .install_targets()
            .into_iter()
            .partition(|t| blockers.iter().any(|b| b == t.spec.as_str()));
        let blocker_plan = self.resolve_plan(session, &pac, &blocker_targets)?;
        let main_plan = self.resolve_plan(session, &pac, &main_targets)?;

        // No table redraw — `show` is where the user looked. `apply` gates on the
        // one-line cost summary plus a single confirm (phase 5a).
        let size_pac = upgrade::synced_pac()?;
        let roots = txn_roots(cart, session, &size_pac);
        let (repo_deps, aur_deps) = merged_dep_rows(main_plan.as_ref(), blocker_plan.as_ref());
        let removals: Vec<PkgName> = cart.removals().to_vec();
        let metrics = upgrade::preview_metrics(session, &roots, main_plan.as_ref());
        ui::info(&ui::cost_summary(
            &roots, &repo_deps, &aur_deps, &removals, &size_pac, &metrics,
        ));
        if !ui::confirm("Proceed with this transaction?", false)
            .map_err(|e| Error::other(format!("confirm: {e}")))?
        {
            return Ok(ApplyOutcome::Declined);
        }

        let mut reviewed = cart.reviewed().clone();
        let opts = InstallOpts {
            noconfirm: false,
            asdeps: false,
            gate: ConfirmGate::AlreadyConfirmed,
        };

        // Blocker rebuilds first — installing them is what unblocks the repo
        // lane (the rebuilt packages no longer carry the dependency the
        // sysupgrade would break).
        let mut report = build::RunReport::default();
        if let Some(plan) = &blocker_plan {
            report = build::apply_plan(self.cfg, session.index(), &pac, plan, opts, &mut reviewed)?;
            if !report.all_landed() {
                // The blocker didn't land, so the repo lane would fail exactly
                // as preflighted — stop before it runs. The repo rows haven't
                // run either, so they stay staged (`repo_landed = false`).
                return Ok(cart_apply_outcome(&report, cart, session, false));
            }
        }

        // Repo upgrades next (before the main AUR builds, so those link against
        // the upgraded libs), via a partial `pacman -Syu` that ignores every
        // repo candidate the user didn't stage.
        if !repo_sel.repo.is_empty() {
            dispatch::run_repo_upgrade(self.cfg, &repo_sel)?;
        }

        // Build + install the main AUR (and any fresh-install) half. Already
        // gated by the confirm above, so `apply_plan` doesn't re-ask.
        if let Some(plan) = &main_plan {
            let main_report =
                build::apply_plan(self.cfg, session.index(), &pac, plan, opts, &mut reviewed)?;
            report.absorb(main_report);
        }
        let outcome = cart_apply_outcome(&report, cart, session, true);
        if !matches!(outcome, ApplyOutcome::Succeeded) {
            return Ok(outcome);
        }

        // Remove half: `pacman -R`, filtered to packages actually installed so a
        // retry after a partial failure doesn't trip on an already-gone target.
        // One atomic add+remove transaction is the phase-6 native-commit goal;
        // until then this is separate transactions bridged by the sudo cache.
        let installed_removals: Vec<&PkgName> = cart
            .removals()
            .iter()
            .filter(|n| pac.is_installed(n.as_str()))
            .collect();
        if !installed_removals.is_empty() {
            // Stringify only here, at pacman's argv boundary.
            let mut args = vec!["-R".to_owned()];
            args.extend(installed_removals.iter().map(|n| n.as_str().to_owned()));
            if invoke::exec_pacman(self.cfg, &args)? != 0 {
                ui::warn("removal step did not complete");
                // The whole install half already landed (we passed the
                // success gate above); only the removal failed, so drop every
                // install row and keep the removals staged for a retry.
                let installed = cart
                    .items()
                    .iter()
                    .map(|it| PkgTarget::new(it.spec()))
                    .collect();
                return Ok(ApplyOutcome::Failed { installed });
            }
        }
        Ok(ApplyOutcome::Succeeded)
    }
}

impl RealEnv<'_> {
    /// Build the build-time overlay for the search table from the metrics store —
    /// only the **installed AUR** rows (build time is a property we show for
    /// installed packages, keyed by pkgname). Empty when there's no session or no
    /// such rows, in which case the table's build column stays blank.
    fn search_metrics(&self, rows: &[ui::SearchRow]) -> ui::PreviewMetrics {
        let Some(session) = &self.session else {
            return ui::PreviewMetrics::empty();
        };
        let roots: Vec<ui::TxnRoot> = rows
            .iter()
            .filter(|r| r.install.installed() && r.repo.rank() == RepoRank::Aur)
            .map(|r| ui::TxnRoot {
                repo: r.repo.clone(),
                approval: ui::ApprovalCell::Approved,
                name: r.name.clone(),
                old_ver: r.old_ver.clone(),
                new_ver: r.new_ver.clone(),
                age: None,
            })
            .collect();
        if roots.is_empty() {
            return ui::PreviewMetrics::empty();
        }
        upgrade::preview_metrics(session, &roots, None)
    }

    /// Re-fetch the mirror + index (subject to `policy`'s TTL) and reload the
    /// session in place, rebuilding the name caches so fresh data backs
    /// subsequent `search`/`info`/classification + completion. Shared by
    /// `upgrade` (which then recomputes candidates) and `refresh` (which stops
    /// here). Invalidates the `show` resolution cache — the mirror/db data it was
    /// resolved against may have just changed.
    fn reload(&mut self, policy: upgrade::FetchPolicy) -> Result<()> {
        let session = upgrade::refresh_and_reload(self.cfg, policy)?;
        self.caches = build_universe(session.as_ref());
        self.session = session;
        self.view = None;
        Ok(())
    }

    /// Build the unified change-set table for `show` from the cached resolution,
    /// re-deriving the approval-bearing root rows from the live cart each call
    /// (cheap — no I/O). [`ui::transaction_table`] returns an owned [`ui::Table`]
    /// (it holds no borrow of the cache), so `render_cart` can print it after the
    /// borrow ends; errors bubble so the caller can fall back to the flat staged
    /// rows — `show` must never abort.
    fn transaction_view(&mut self, cart: &Cart) -> Result<ui::Table> {
        self.ensure_view(cart)?;
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::other("no AUR index loaded"))?;
        let r = &self
            .view
            .as_ref()
            .expect("ensure_view populated the cache on Ok")
            .resolved;
        let roots = txn_roots(cart, session, &r.size_pac);
        let removals: Vec<PkgName> = cart.removals().to_vec();
        Ok(ui::transaction_table(
            &roots,
            &r.repo_deps,
            &r.aur_deps,
            &removals,
            &r.size_pac,
            &r.metrics,
            ui::Paint::detect(),
        ))
    }

    /// Ensure [`Self::view`] holds a resolution valid for `cart`, re-resolving
    /// only on a package-set change (the [`TxnKey`]) — a reload/apply already
    /// cleared it. Propagates a resolve error so [`Self::transaction_view`] can
    /// fall back to flat rows without caching the failure.
    fn ensure_view(&mut self, cart: &Cart) -> Result<()> {
        let key = TxnKey::of(cart);
        if self.view.as_ref().is_some_and(|v| v.key == key) {
            return Ok(());
        }
        let resolved = self.resolve_view(cart)?;
        self.view = Some(CachedTxn { key, resolved });
        Ok(())
    }

    /// Resolve the cart's package set into a [`ResolvedTxn`]: run the dependency
    /// resolve, then from its plan derive the synced-db size snapshot, the
    /// pulled-in dep rows, and the build-time overlay (the plan itself isn't
    /// kept). This is the expensive I/O the cache amortises (`resolve_targets` +
    /// two alpm opens + the metrics store), recomputed only on a package-set
    /// change or a reload/apply.
    fn resolve_view(&self, cart: &Cart) -> Result<ResolvedTxn> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::other("no AUR index loaded"))?;
        let pac = upgrade::system_pac()?;
        let plan = self.resolve_plan(session, &pac, &cart.install_targets())?;
        // Sizes from the freshly-synced db (the new versions' real download cost).
        let size_pac = upgrade::synced_pac()?;
        let (repo_deps, aur_deps) = upgrade::dep_rows(plan.as_ref());
        // Roots feed only the (approval-independent) build-time overlay here; the
        // render re-derives approval-aware roots from the live cart.
        let roots = txn_roots(cart, session, &size_pac);
        let metrics = upgrade::preview_metrics(session, &roots, plan.as_ref());
        let preflight = self.preview_preflight(session, cart);
        Ok(ResolvedTxn {
            size_pac,
            repo_deps,
            aur_deps,
            metrics,
            preflight,
        })
    }

    /// The sysupgrade preflight for the `show` preview — no db re-sync (`show`
    /// must stay instant; the drift guard belongs to `apply`) and no gating,
    /// just the notes to render under the table. Empty when the cart stages no
    /// repo upgrades or the check couldn't run (best-effort, like every other
    /// preflight consumer).
    fn preview_preflight(
        &self,
        session: &UpgradeSession,
        cart: &Cart,
    ) -> Vec<upgrade::PreflightNote> {
        if cart.repo_upgrades().is_empty() {
            return Vec::new();
        }
        let sel = match self.repo_upgrade_selection(session, cart) {
            Ok(sel) => sel,
            Err(e) => {
                debug!(error = %e, "preview preflight skipped (upgrade selection failed)");
                return Vec::new();
            }
        };
        if sel.repo.is_empty() {
            return Vec::new();
        }
        match preflight::sysupgrade(&sel.repo_skipped) {
            Ok(issues) => upgrade::preflight_notes(issues, session, cart),
            Err(e) => {
                debug!(error = %e, "preview preflight skipped (could not run prepare)");
                Vec::new()
            }
        }
    }

    /// Resolve `targets` (the cart's install/build half, or a phase-subset of
    /// it) into a [`Plan`] — the AUR rows and fresh installs (repo *upgrades*
    /// take the `-Syu` lane, so they are never targets here). `None` when the
    /// subset is empty (a repo-upgrade-only or removal-only cart / phase).
    fn resolve_plan(
        &self,
        session: &UpgradeSession,
        pac: &PacmanIndex,
        targets: &[build::Target],
    ) -> Result<Option<Plan>> {
        if targets.is_empty() {
            return Ok(None);
        }
        let plan = build::resolve_targets(
            self.cfg,
            session.index(),
            Some(session.secondary()),
            pac,
            targets,
            false,
        )?;
        Ok(Some(plan))
    }

    /// Turn the staged repo upgrades into the partial-`-Syu` selection: the
    /// staged ones are upgraded; every other current repo candidate is
    /// `--ignore`d. Recomputes the candidate set so a stale cart can't pin the
    /// wrong packages.
    fn repo_upgrade_selection(
        &self,
        session: &UpgradeSession,
        cart: &Cart,
    ) -> Result<UpgradeSelection> {
        let staged: HashSet<PkgName> = cart
            .repo_upgrades()
            .iter()
            .map(|u| u.name.clone())
            .collect();
        let mut repo = Vec::new();
        let mut repo_skipped = Vec::new();
        for u in session
            .recompute_remaining(self.devel)?
            .into_iter()
            .filter(|u| u.repo != REPO_AUR)
        {
            if staged.contains(&u.name) {
                repo.push(u.name);
            } else {
                repo_skipped.push(u.name);
            }
        }
        Ok(UpgradeSelection {
            repo,
            repo_skipped,
            aur: Vec::new(),
        })
    }
}

/// Fold a build [`RunReport`](build::RunReport) into the cart-apply outcome. A
/// fully-clean report succeeds (the caller clears the whole cart); any failure,
/// dep-skip, or interrupt is a partial [`ApplyOutcome::Failed`] carrying the
/// staged install rows that *did* land — so the cart drops them and keeps only
/// the offenders for a retry (fixing the bug where a partial build left every
/// package staged even though some installed).
///
/// Which rows landed: each AUR row whose pkgbase built + installed this run
/// (`report.installed`), plus — when `repo_landed` — every repo row. The repo
/// lanes run to completion before the *main* AUR pipeline (a repo failure
/// surfaces as `Err`, never this outcome), so `repo_landed` is true there; a
/// failure in the *blocker* phase stops the apply before any repo lane ran, so
/// that caller passes false and the repo rows stay staged.
fn cart_apply_outcome(
    report: &build::RunReport,
    cart: &Cart,
    session: &UpgradeSession,
    repo_landed: bool,
) -> ApplyOutcome {
    if report.all_landed() {
        return ApplyOutcome::Succeeded;
    }
    let installed = landed_install_specs(cart, &report.installed, repo_landed, |it| {
        session
            .secondary()
            .lookup(session.index(), it.spec())
            .map(|e| e.pkgbase.clone())
    });
    ApplyOutcome::Failed { installed }
}

/// The staged install specs that landed this run, given the AUR pkgbases that
/// installed and a resolver from a cart row to its pkgbase. Repo rows count as
/// landed iff the repo lanes ran (`repo_landed` — see [`cart_apply_outcome`]);
/// an AUR row lands iff its pkgbase is in `installed`. Pure (the pkgbase lookup
/// is injected) so the partition is unit-testable without a live session.
fn landed_install_specs(
    cart: &Cart,
    installed: &[PkgBase],
    repo_landed: bool,
    pkgbase_of: impl Fn(&CartItem) -> Option<PkgBase>,
) -> Vec<PkgTarget> {
    cart.items()
        .iter()
        .filter(|it| match it.source {
            Source::Repo => repo_landed,
            Source::Aur => pkgbase_of(it).is_some_and(|pb| installed.contains(&pb)),
        })
        .map(|it| PkgTarget::new(it.spec()))
        .collect()
}

/// Run the read-only sysupgrade preflight for the staged repo lane and gate on
/// what it finds — before the cost summary, the transaction confirm, and the
/// sudo prompt, so a doomed `pacman -Syu` never gets that far.
///
/// `Ok(Some(blockers))` means proceed: `blockers` are the staged AUR rebuilds
/// that resolve flagged breakage and must install ahead of the repo lane.
/// `Ok(None)` means the user declined at the override prompt.
fn sysupgrade_gate(
    session: &UpgradeSession,
    cart: &Cart,
    sel: &UpgradeSelection,
) -> Result<Option<Vec<PkgName>>> {
    let issues = match preflight::sysupgrade(&sel.repo_skipped) {
        Ok(issues) => issues,
        // Best-effort: if the preflight machinery itself can't run, pacman
        // remains the authority — proceed exactly as before it existed.
        Err(e) => {
            debug!(error = %e, "sysupgrade preflight skipped (could not run prepare)");
            return Ok(Some(Vec::new()));
        }
    };
    if issues.is_empty() {
        return Ok(Some(Vec::new()));
    }
    // The same structured events the passthrough lane logs, then the
    // human-facing notes with remediation.
    preflight::log_issues(&issues);
    let notes = upgrade::preflight_notes(issues, session, cart);
    let mut blockers = Vec::new();
    let mut blocking = false;
    for note in &notes {
        upgrade::print_preflight_note(note);
        match &note.remedy {
            upgrade::Remedy::StagedRebuild { target } => blockers.push(target.clone()),
            _ => blocking = true,
        }
    }
    if !blocking {
        return Ok(Some(blockers));
    }
    // Advisory, not authoritative: the synced snapshot can trail the mirror
    // pacman is about to fetch from, so offer the override — but walking away
    // must mean no.
    if ui::confirm_default_no("Repo upgrade expected to fail — run pacman anyway?")
        .map_err(|e| Error::other(format!("confirm: {e}")))?
    {
        Ok(Some(blockers))
    } else {
        Ok(None)
    }
}

/// Dep rows for the cost summary when the install half is split into blocker +
/// main phases: the union of both plans' rows, deduped (a dep shared by the two
/// plans is built/installed once).
fn merged_dep_rows(main: Option<&Plan>, blocker: Option<&Plan>) -> (Vec<PkgName>, Vec<PkgBase>) {
    let (mut repo_deps, mut aur_deps) = upgrade::dep_rows(main);
    let (blocker_repo, blocker_aur) = upgrade::dep_rows(blocker);
    for d in blocker_repo {
        if !repo_deps.contains(&d) {
            repo_deps.push(d);
        }
    }
    for d in blocker_aur {
        if !aur_deps.contains(&d) {
            aur_deps.push(d);
        }
    }
    (repo_deps, aur_deps)
}

/// Map the cart's [`Approval`] to the renderer's presentation enum — the seam
/// that keeps `ui::change_set` from depending on `cli::shell`.
const fn approval_cell(approval: Approval) -> ui::ApprovalCell {
    match approval {
        Approval::Approved => ui::ApprovalCell::Approved,
        Approval::NeedsReview => ui::ApprovalCell::NeedsReview,
    }
}

/// Build the numbered root rows for the unified table from the (sorted) cart,
/// resolving each row's version pair and AUR age. `size_pac` is the synced
/// snapshot — it carries the new repo versions for fresh repo installs.
fn txn_roots(cart: &Cart, session: &UpgradeSession, size_pac: &PacmanIndex) -> Vec<ui::TxnRoot> {
    let now = SystemTime::now();
    cart.items()
        .iter()
        .map(|it| {
            let (old_ver, new_ver) = row_versions(it, session, size_pac);
            ui::TxnRoot {
                repo: it.repo_label(),
                approval: approval_cell(it.approval),
                name: PkgName::from(it.spec()),
                old_ver,
                new_ver,
                age: aur_age(it, session, now),
            }
        })
        .collect()
}

/// The `(old, new)` versions for a row: an upgrade carries both; a fresh install
/// has no `old` and takes `new` from the AUR index (AUR rows) or the synced
/// syncdb (repo rows). Either fresh lookup may miss → `None` (the renderer then
/// leaves the version cell blank but aligned).
fn row_versions(
    it: &CartItem,
    session: &UpgradeSession,
    size_pac: &PacmanIndex,
) -> (Option<Version>, Option<Version>) {
    if let Some(u) = &it.upgrade {
        return (Some(u.old_ver.clone()), Some(u.new_ver.clone()));
    }
    let new = match it.source {
        Source::Aur => session
            .secondary()
            .lookup(session.index(), it.spec())
            .map(IndexEntry::version),
        Source::Repo => size_pac.sync_version(it.spec()).map(Version::from),
    };
    (None, new)
}

/// The AUR pkgbase's "last modified" age (its branch-tip commit time vs `now`),
/// for the table's age column. `None` for repo rows, when there's no matching
/// index entry, or when the commit time is unrecorded (the
/// [`crate::units::UnixTime`] sentinel in archives predating the field).
fn aur_age(it: &CartItem, session: &UpgradeSession, now: SystemTime) -> Option<Duration> {
    if it.source != Source::Aur {
        return None;
    }
    let entry = session.secondary().lookup(session.index(), it.spec())?;
    now.duration_since(entry.commit_time.system_time()?).ok()
}

/// The graceful-degradation rendering for `show` when the resolve behind the
/// unified table fails (unknown target, mirror gap): a note plus the flat staged
/// rows, so `show` still tells the user what's in the cart instead of erroring.
fn flat_cart_lines(cart: &Cart, err: &Error) -> Vec<String> {
    let mut out = vec![format!(
        "  (couldn't resolve the full change set: {err} — showing staged items)"
    )];
    for (i, it) in cart.items().iter().enumerate() {
        let ver = it
            .version_transition()
            .map_or_else(String::new, |t| format!("  {t}"));
        out.push(format!(
            "{:>3}  {}  {}  {}{ver}",
            i + 1,
            it.repo_label(),
            it.approval.label(),
            it.spec(),
        ));
    }
    for name in cart.removals() {
        out.push(format!("     remove  {name}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    /// The fake env's captured output: every `print`ed line, in order.
    ///
    /// A named domain type rather than a bare `Vec<String>` — but deliberately
    /// *not* [`ui::Table`], which is specifically *rendered-table* lines built
    /// only inside `ui` (and whose own doc warns against conflating it with other
    /// string lists). This is a transcript of arbitrary shell output, exposing
    /// the substring assertions the tests actually make rather than a raw `Vec`.
    #[derive(Default, Debug)]
    struct Transcript(Vec<String>);

    impl Transcript {
        fn push(&mut self, line: &str) {
            self.0.push(line.to_owned());
        }
        fn clear(&mut self) {
            self.0.clear();
        }
        fn is_empty(&self) -> bool {
            self.0.is_empty()
        }
        /// Some printed line contains `needle` — the common assertion.
        fn contains(&self, needle: &str) -> bool {
            self.0.iter().any(|l| l.contains(needle))
        }
        /// Some printed line satisfies `pred`, for compound / exact-match checks.
        fn any(&self, pred: impl Fn(&str) -> bool) -> bool {
            self.0.iter().any(|l| pred(l))
        }
        /// The whole transcript as one string, for cross-line substring checks.
        fn joined(&self) -> String {
            self.0.join("\n")
        }
    }

    /// How many times a scripted env effect ran. A typed counter so the fake's
    /// call bookkeeping reads `env.upgrades.count()` against a named type instead
    /// of a bare `usize` that could be compared against anything.
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
    struct CallCount(usize);

    impl CallCount {
        fn bump(&mut self) {
            self.0 += 1;
        }
        fn count(self) -> usize {
            self.0
        }
    }

    /// Scripted [`ShellEnv`] capturing output + recording calls, with a
    /// pre-seeded search result, name universe, classification table, and
    /// scripted review/apply outcomes, so dispatch is testable without a
    /// terminal, index, or alpm.
    #[derive(Default)]
    struct FakeEnv {
        lines: Transcript,
        upgrades: CallCount,
        refreshes: CallCount,
        search_result: Vec<ListItem>,
        info_calls: Vec<Vec<PkgTarget>>,
        names: Vec<PkgTarget>,
        /// What `upgrade` returns (the recomputed candidates to seed).
        upgrade_candidates: Vec<PkgUpgrade>,
        /// spec → coarse source; absent ⇒ `classify` returns `None`.
        classes: HashMap<String, Source>,
        policy: AurApproval,
        /// spec → review verdict; absent ⇒ `Approved`.
        review_outcomes: HashMap<String, ReviewOutcome>,
        review_calls: Vec<String>,
        /// What `apply` returns; absent ⇒ `Succeeded`.
        apply_outcome: Option<ApplyOutcome>,
        apply_calls: CallCount,
    }

    impl ShellEnv for FakeEnv {
        fn print(&mut self, line: &str) {
            self.lines.push(line);
        }
        fn upgrade(&mut self) -> Result<Vec<PkgUpgrade>> {
            self.upgrades.bump();
            Ok(self.upgrade_candidates.clone())
        }
        fn refresh(&mut self) -> Result<()> {
            self.refreshes.bump();
            Ok(())
        }
        fn search(&mut self, _terms: &[SearchTerm]) -> Result<Vec<ListItem>> {
            Ok(self.search_result.clone())
        }
        fn show_info(&mut self, targets: &[PkgTarget]) -> Result<()> {
            self.info_calls.push(targets.to_vec());
            Ok(())
        }
        fn names(&self) -> &[PkgTarget] {
            &self.names
        }
        fn classify(&self, target: &PkgTarget) -> Option<StageClass> {
            // The fake tracks only the coarse source; the concrete repo (which
            // drives the display + `drop core`) is exercised via `upgrade`-seeded
            // rows, whose `from_upgrade` carries the real repo name.
            self.classes
                .get(target.as_str())
                .map(|&source| StageClass { source, repo: None })
        }
        fn aur_policy(&self) -> AurApproval {
            self.policy
        }
        fn pkgbase_of(&self, target: &PkgTarget) -> Option<PkgBase> {
            Some(PkgBase::from(target.as_str()))
        }
        fn review(&mut self, target: &PkgTarget) -> Result<ReviewOutcome> {
            self.review_calls.push(target.as_str().to_owned());
            Ok(self
                .review_outcomes
                .remove(target.as_str())
                .unwrap_or(ReviewOutcome::Approved))
        }
        fn render_cart(&mut self, _cart: &Cart) {
            // The table rendering (color, alignment, age) is RealEnv's job; the
            // pure dispatch core under test prints the header + summary itself.
        }
        fn apply(&mut self, _cart: &Cart) -> Result<ApplyOutcome> {
            self.apply_calls.bump();
            Ok(self.apply_outcome.take().unwrap_or(ApplyOutcome::Succeeded))
        }
    }

    /// A `FakeEnv` that classifies the given specs (everything else is unknown).
    fn env_with(classes: &[(&str, Source)]) -> FakeEnv {
        let mut env = FakeEnv::default();
        for (spec, source) in classes {
            env.classes.insert((*spec).to_owned(), *source);
        }
        env
    }

    fn li(label: &str, name: &str) -> ListItem {
        ListItem {
            target: PkgTarget::new(name),
            label: label.to_owned(),
            repo: None,
        }
    }

    /// A list row tagged with its repo, for the `add <repo>` filter tests.
    fn li_repo(repo: &str, name: &str) -> ListItem {
        ListItem {
            target: PkgTarget::new(name),
            label: format!("{repo}/{name} 1-1"),
            repo: Some(RepoName::from(repo)),
        }
    }

    fn dispatch_one(input: &str) -> (Flow, FakeEnv) {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        let flow = state.dispatch(&command::parse(input), &mut env);
        (flow, env)
    }

    #[test]
    fn quit_and_aliases_exit_zero() {
        assert_eq!(dispatch_one("quit").0, Flow::Exit(0));
        assert_eq!(dispatch_one("exit").0, Flow::Exit(0));
        assert_eq!(dispatch_one("q").0, Flow::Exit(0));
    }

    #[test]
    fn empty_line_continues_with_no_output() {
        let (flow, env) = dispatch_one("   ");
        assert_eq!(flow, Flow::Continue);
        assert!(
            env.lines.is_empty(),
            "blank line prints nothing: {:?}",
            env.lines
        );
    }

    #[test]
    fn unknown_command_points_at_help() {
        let (flow, env) = dispatch_one("frobnicate x");
        assert_eq!(flow, Flow::Continue);
        assert!(
            env.lines
                .any(|l| l.contains("unknown command") && l.contains("frobnicate")),
            "got: {:?}",
            env.lines
        );
    }

    #[test]
    fn help_lists_the_core_verbs() {
        let (flow, env) = dispatch_one("help");
        assert_eq!(flow, Flow::Continue);
        let joined = env.lines.joined();
        for verb in ["search", "info", "add", "upgrade", "apply", "quit"] {
            assert!(joined.contains(verb), "help text missing `{verb}`");
        }
    }

    #[test]
    fn help_topic_prints_the_single_command_detail() {
        let (flow, env) = dispatch_one("help add");
        assert_eq!(flow, Flow::Continue);
        let joined = env.lines.joined();
        assert!(joined.contains("add <sel…>"), "got: {joined}");
        // The detail names `add`'s alias and its resolution scope, which the
        // one-line overview doesn't.
        assert!(joined.contains("install"), "add topic omits its alias");
        // It's the topic, not the whole list — an unrelated verb's body is absent.
        assert!(!joined.contains("Leave the shell"), "printed the full list");
    }

    #[test]
    fn help_topic_resolves_aliases() {
        // `discard` is an alias for `drop`; `help discard` shows drop's topic.
        let (_, env) = dispatch_one("help discard");
        assert!(env.lines.contains("drop <sel…>"), "got: {:?}", env.lines);
    }

    #[test]
    fn help_unknown_topic_points_back_at_help() {
        let (_, env) = dispatch_one("help frobnicate");
        assert!(
            env.lines
                .any(|l| l.contains("no help") && l.contains("frobnicate")),
            "got: {:?}",
            env.lines
        );
    }

    #[test]
    fn every_verb_has_a_help_topic() {
        // Guards TOPICS against drifting from the verb set: a new verb without a
        // topic (or a renamed one) fails here rather than printing "no help".
        for verb in command::VERBS {
            assert!(
                TOPICS.iter().any(|(v, _)| v == verb),
                "no `help {verb}` topic",
            );
        }
    }

    fn up(repo: &str, name: &str) -> PkgUpgrade {
        use crate::version::Version;
        PkgUpgrade {
            repo: RepoName::from(repo),
            name: PkgName::from(name),
            old_ver: Version::from("1-1"),
            new_ver: Version::from("2-1"),
        }
    }

    #[test]
    fn upgrade_seeds_the_cart_repo_approved_aur_needs_review() {
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("core", "glibc"), up("aur", "yay-bin")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env);
        assert_eq!(env.upgrades.count(), 1, "upgrade recomputes once");
        assert_eq!(state.cart.items().len(), 2, "both candidates staged");
        // Repo upgrade auto-approves; AUR upgrade needs review.
        assert_eq!(state.cart.pending_review().len(), 1);
        assert_eq!(state.cart.pending_review()[0].spec(), "yay-bin");
    }

    #[test]
    fn upgrade_with_selector_seeds_only_the_subset() {
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("core", "glibc"), up("aur", "yay-bin")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade yay-bin"), &mut env);
        let specs: Vec<&str> = state.cart.items().iter().map(CartItem::spec).collect();
        assert_eq!(specs, vec!["yay-bin"]);
    }

    #[test]
    fn upgrade_with_nothing_to_do_stages_nothing() {
        let (flow, env) = dispatch_one("upgrade");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.contains("nothing to upgrade"));
    }

    #[test]
    fn refresh_reloads_without_seeding_or_touching_the_cart() {
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        state.dispatch(&command::parse("refresh"), &mut env);
        assert_eq!(env.refreshes.count(), 1, "refresh re-fetches once");
        assert_eq!(
            env.upgrades.count(),
            0,
            "refresh is not an upgrade recompute"
        );
        assert_eq!(
            state.cart.items().len(),
            1,
            "refresh leaves the cart intact"
        );
        assert!(env.lines.contains("refreshed"));
    }

    #[test]
    fn add_stages_with_source_and_default_approval() {
        let mut env = env_with(&[("glibc", Source::Repo), ("yay-bin", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add glibc yay-bin"), &mut env);
        assert_eq!(state.cart.items().len(), 2);
        // Repo auto-approves; AUR needs review.
        assert!(!state.cart.all_approved());
        assert_eq!(state.cart.pending_review().len(), 1);
        assert_eq!(state.cart.pending_review()[0].spec(), "yay-bin");
    }

    #[test]
    fn add_unknown_package_is_not_staged() {
        let mut env = FakeEnv::default(); // classifies nothing
        let mut state = State::default();
        state.dispatch(&command::parse("add nope"), &mut env);
        assert!(state.cart.is_empty());
        assert!(env.lines.contains("unknown package"));
    }

    #[test]
    fn add_dedups_silently() {
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        state.dispatch(&command::parse("add foo"), &mut env);
        assert_eq!(state.cart.items().len(), 1);
        assert!(env.lines.contains("already staged"));
    }

    #[test]
    fn drop_unstages_a_cart_row() {
        let mut env = env_with(&[("foo", Source::Aur), ("bar", Source::Repo)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar"), &mut env);
        state.dispatch(&command::parse("drop foo"), &mut env);
        let specs: Vec<&str> = state.cart.items().iter().map(CartItem::spec).collect();
        assert_eq!(specs, vec!["bar"]);
    }

    #[test]
    fn keep_drops_every_unselected_install() {
        let mut env = env_with(&[
            ("foo", Source::Aur),
            ("bar", Source::Repo),
            ("baz", Source::Aur),
        ]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar baz"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("keep bar"), &mut env);
        assert_eq!(cart_specs(&state), vec!["bar"], "only `bar` survives");
        assert!(env.lines.contains("dropped foo"));
        assert!(env.lines.contains("dropped baz"));
        // Reprints the narrowed cart, like `drop`.
        assert!(env.lines.contains("transaction — 1 to install"));
    }

    #[test]
    fn keep_by_repo_filter_narrows_to_one_repo() {
        // A repo-name selector keeps every row from that repo — the mirror image
        // of `drop <repo>`.
        let mut env = FakeEnv {
            upgrade_candidates: vec![
                up("core", "glibc"),
                up("extra", "firefox"),
                up("aur", "yay-bin"),
            ],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env);
        state.dispatch(&command::parse("keep aur"), &mut env);
        assert_eq!(cart_specs(&state), vec!["yay-bin"]);
    }

    #[test]
    fn keep_matching_nothing_leaves_the_cart_intact() {
        // A typo mustn't empty the cart: no staged row matches → no change.
        let mut env = env_with(&[("foo", Source::Aur), ("bar", Source::Repo)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("keep absent"), &mut env);
        assert_eq!(cart_specs(&state), vec!["bar", "foo"], "cart unchanged");
        assert!(env.lines.contains("nothing in the cart matched"));
        assert!(
            !env.lines.any(|l| l.contains("dropped")),
            "no row should be dropped: {:?}",
            env.lines
        );
    }

    #[test]
    fn keep_with_no_args_prints_usage() {
        let (flow, env) = dispatch_one("keep");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.contains("usage: keep"));
    }

    #[test]
    fn clear_empties_the_cart() {
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        state.dispatch(&command::parse("clear"), &mut env);
        assert!(state.cart.is_empty());
    }

    #[test]
    fn approve_clears_the_gate_without_review() {
        let mut env = env_with(&[("yay-bin", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add yay-bin"), &mut env);
        state.dispatch(&command::parse("approve yay-bin"), &mut env);
        assert!(state.cart.all_approved());
        assert!(env.review_calls.is_empty(), "approve opens no diff");
        // The pkgbase is recorded so apply won't re-prompt the build pipeline.
        assert!(state.cart.reviewed().contains(&PkgBase::from("yay-bin")));
    }

    #[test]
    fn approve_glob_star_approves_every_staged_aur_item() {
        let mut env = env_with(&[("a", Source::Aur), ("b", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add a b"), &mut env);
        state.dispatch(&command::parse("approve *"), &mut env);
        assert!(state.cart.all_approved());
    }

    #[test]
    fn review_approves_on_an_approved_outcome() {
        let mut env = env_with(&[("yay-bin", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add yay-bin"), &mut env);
        state.dispatch(&command::parse("review yay-bin"), &mut env);
        assert_eq!(env.review_calls, vec!["yay-bin"], "review opened the diff");
        assert!(state.cart.all_approved());
    }

    #[test]
    fn review_skip_leaves_item_pending() {
        let mut env = env_with(&[("yay-bin", Source::Aur)]);
        env.review_outcomes
            .insert("yay-bin".into(), ReviewOutcome::Skipped);
        let mut state = State::default();
        state.dispatch(&command::parse("add yay-bin"), &mut env);
        state.dispatch(&command::parse("review yay-bin"), &mut env);
        assert!(!state.cart.all_approved(), "skip leaves it needing review");
    }

    #[test]
    fn review_approve_all_clears_the_rest_without_more_diffs() {
        // The `(a)pprove all` outcome on the first item auto-approves the rest
        // without opening their diffs.
        let mut env = env_with(&[("a", Source::Aur), ("b", Source::Aur), ("c", Source::Aur)]);
        env.review_outcomes
            .insert("a".into(), ReviewOutcome::ApprovedAll);
        let mut state = State::default();
        state.dispatch(&command::parse("add a b c"), &mut env);
        state.dispatch(&command::parse("review"), &mut env);
        assert_eq!(
            env.review_calls,
            vec!["a"],
            "approve-all opens only the first diff, then auto-approves the rest"
        );
        assert!(state.cart.all_approved(), "every staged item is cleared");
    }

    #[test]
    fn review_without_args_reviews_every_pending_item() {
        let mut env = env_with(&[("a", Source::Aur), ("b", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add a b"), &mut env);
        state.dispatch(&command::parse("review"), &mut env);
        assert_eq!(
            env.review_calls,
            vec!["a", "b"],
            "bare `review` opens every pending item"
        );
        assert!(state.cart.all_approved(), "approving all clears the gate");
    }

    #[test]
    fn review_without_args_skips_already_approved_items() {
        // A repo package auto-approves, so a bare `review` has nothing to do.
        let mut env = env_with(&[("glibc", Source::Repo)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add glibc"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("review"), &mut env);
        assert!(env.review_calls.is_empty(), "nothing pending to open");
        assert!(env.lines.contains("nothing to review"));
    }

    #[test]
    fn apply_gate_blocks_while_items_need_review() {
        let mut env = env_with(&[("yay-bin", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add yay-bin"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env);
        assert_eq!(
            env.apply_calls.count(),
            0,
            "apply must not run while pending"
        );
        assert!(env.lines.contains("needs review"));
    }

    #[test]
    fn apply_runs_when_all_approved_and_clears_on_success() {
        let mut env = env_with(&[("glibc", Source::Repo)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add glibc"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env);
        assert_eq!(env.apply_calls.count(), 1);
        assert!(state.cart.is_empty(), "a clean apply clears the cart");
        assert!(env.lines.any(|l| l == "done"));
    }

    #[test]
    fn apply_declined_keeps_the_cart() {
        let mut env = env_with(&[("glibc", Source::Repo)]);
        env.apply_outcome = Some(ApplyOutcome::Declined);
        let mut state = State::default();
        state.dispatch(&command::parse("add glibc"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env);
        assert_eq!(state.cart.items().len(), 1, "declined apply keeps the cart");
    }

    #[test]
    fn apply_total_failure_keeps_the_whole_cart_for_retry() {
        let mut env = env_with(&[("glibc", Source::Repo)]);
        // Nothing landed → empty `installed` → the whole cart stays staged.
        env.apply_outcome = Some(ApplyOutcome::Failed {
            installed: Vec::new(),
        });
        let mut state = State::default();
        state.dispatch(&command::parse("add glibc"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env);
        assert_eq!(state.cart.items().len(), 1, "failed apply keeps the cart");
        assert!(env.lines.contains("cart kept for retry"));
    }

    #[test]
    fn apply_partial_failure_drops_landed_rows_and_keeps_the_failures() {
        // Regression: `upgrade` stages 4 AUR packages, 2 build + install and 2
        // fail. The cart must keep only the 2 that failed — not show all 4.
        let mut env = env_with(&[
            ("a", Source::Aur),
            ("b", Source::Aur),
            ("c", Source::Aur),
            ("d", Source::Aur),
        ]);
        // `a` and `b` landed; `c` and `d` didn't.
        env.apply_outcome = Some(ApplyOutcome::Failed {
            installed: vec![PkgTarget::new("a"), PkgTarget::new("b")],
        });
        let mut state = State::default();
        state.dispatch(&command::parse("add a b c d"), &mut env);
        state.dispatch(&command::parse("approve *"), &mut env); // clear the gate
        state.dispatch(&command::parse("apply"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["c", "d"],
            "only the failed packages stay staged"
        );
        assert!(env.lines.contains("apply partly failed"));
    }

    #[test]
    fn remove_stages_an_uninstall() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("remove oldpkg"), &mut env);
        assert_eq!(state.cart.removals(), &[PkgName::from("oldpkg")]);
    }

    #[test]
    fn landed_install_specs_keeps_repo_and_installed_aur_only() {
        // A mixed cart: one repo upgrade, two AUR. On a partial failure the repo
        // row always counts as landed, and an AUR row lands iff its pkgbase is in
        // the report's `installed` set.
        let mut cart = Cart::default();
        cart.add(CartItem::from_upgrade(
            up("core", "glibc"),
            AurApproval::Review,
        ));
        cart.add(CartItem::from_upgrade(
            up("aur", "yay-bin"),
            AurApproval::Review,
        ));
        cart.add(CartItem::from_upgrade(
            up("aur", "cuda"),
            AurApproval::Review,
        ));
        // `yay-bin` built + installed; `cuda` did not.
        let installed = [PkgBase::from("yay-bin")];
        // The fixtures use spec == pkgbase, so an identity resolver suffices.
        let pkgbase_of = |it: &CartItem| Some(PkgBase::from(it.spec()));
        let landed = landed_install_specs(&cart, &installed, true, pkgbase_of);
        let specs: Vec<&str> = landed.iter().map(PkgTarget::as_str).collect();
        assert_eq!(
            specs,
            vec!["glibc", "yay-bin"],
            "repo row + installed AUR landed; the failed AUR (`cuda`) did not"
        );
        // A blocker-phase failure stops the apply before any repo lane runs —
        // with `repo_landed = false` the repo row must stay staged.
        let landed = landed_install_specs(&cart, &installed, false, pkgbase_of);
        let specs: Vec<&str> = landed.iter().map(PkgTarget::as_str).collect();
        assert_eq!(
            specs,
            vec!["yay-bin"],
            "without the repo lane having run, only the installed AUR row landed"
        );
    }

    #[test]
    fn merged_dep_rows_unions_and_dedupes_the_two_plans() {
        // Main plan: pulls openssl+zlib from the repos, and AUR pkgbase
        // `libfoo-git` as a dep (`yay-bin` is a named root, so not a dep row).
        let main = Plan {
            transitive_repo: vec!["openssl".to_owned(), "zlib".to_owned()],
            aur_strata: vec![vec![PkgBase::from("yay-bin"), PkgBase::from("libfoo-git")]],
            direct_aur: std::iter::once(PkgBase::from("yay-bin")).collect(),
            ..Plan::default()
        };
        // Blocker plan: shares zlib with the main plan and adds its own rows.
        let blocker = Plan {
            transitive_repo: vec!["zlib".to_owned(), "libjpeg-turbo".to_owned()],
            aur_strata: vec![vec![PkgBase::from("libbar-git")]],
            ..Plan::default()
        };
        let (repo, aur) = merged_dep_rows(Some(&main), Some(&blocker));
        let repo: Vec<&str> = repo.iter().map(PkgName::as_str).collect();
        assert_eq!(
            repo,
            vec!["openssl", "zlib", "libjpeg-turbo"],
            "shared zlib appears once, main-plan order first"
        );
        let aur: Vec<&str> = aur.iter().map(PkgBase::as_str).collect();
        assert_eq!(aur, vec!["libfoo-git", "libbar-git"]);

        // A blocker-only apply (repo-upgrade cart with no main AUR half).
        let (repo, aur) = merged_dep_rows(None, Some(&blocker));
        let repo: Vec<&str> = repo.iter().map(PkgName::as_str).collect();
        assert_eq!(repo, vec!["zlib", "libjpeg-turbo"]);
        assert_eq!(aur.len(), 1);
    }

    #[test]
    fn show_reports_pending_then_ready() {
        let mut env = env_with(&[("yay-bin", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add yay-bin"), &mut env);
        state.dispatch(&command::parse("show"), &mut env);
        assert!(env.lines.contains("need review"));
        state.dispatch(&command::parse("approve yay-bin"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("show"), &mut env);
        assert!(env.lines.contains("all approved"));
    }

    #[test]
    fn add_reprints_the_whole_cart() {
        // A successful stage reprints the transaction so the user sees the cart
        // without typing `show` (post-5c UX). The pure core's `show` header is
        // the observable proof here (the table body is RealEnv's job).
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        assert!(
            env.lines.contains("transaction — 1 to install"),
            "add should reprint the cart: {:?}",
            env.lines
        );
    }

    #[test]
    fn add_no_op_stays_quiet() {
        // An add that stages nothing (unknown package) must not reprint the cart.
        let mut env = FakeEnv::default(); // classifies nothing
        let mut state = State::default();
        state.dispatch(&command::parse("add nope"), &mut env);
        assert!(
            !env.lines.any(|l| l.contains("transaction —")),
            "a no-op add should not reprint: {:?}",
            env.lines
        );
    }

    #[test]
    fn drop_reprints_the_remaining_cart() {
        let mut env = env_with(&[("foo", Source::Aur), ("bar", Source::Repo)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("drop foo"), &mut env);
        assert!(
            env.lines.contains("transaction — 1 to install"),
            "drop should reprint the remaining cart: {:?}",
            env.lines
        );
    }

    #[test]
    fn remove_reprints_the_cart_with_the_removal_row() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("remove oldpkg"), &mut env);
        assert!(
            env.lines
                .contains("transaction — 0 to install, 1 to remove"),
            "remove should reprint the cart: {:?}",
            env.lines
        );
    }

    #[test]
    fn syntax_error_is_reported_not_fatal() {
        let (flow, env) = dispatch_one("add \"unterminated");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.contains("syntax error"), "got: {:?}", env.lines);
    }

    #[test]
    fn search_prints_numbered_list_and_remembers_it() {
        let mut env = FakeEnv {
            search_result: vec![li("aur/foo 1-1", "foo"), li("extra/bar 2-1", "bar")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        let flow = state.dispatch(&command::parse("search foo"), &mut env);
        assert_eq!(flow, Flow::Continue);
        assert!(
            env.lines
                .any(|l| l.starts_with("  1") && l.contains("aur/foo")),
            "row 1 should be numbered: {:?}",
            env.lines
        );
        assert!(
            env.lines
                .any(|l| l.contains("  2") && l.contains("extra/bar"))
        );
        assert_eq!(state.search_list.len(), 2, "the list should be remembered");
    }

    #[test]
    fn search_with_no_terms_prints_usage() {
        let (flow, env) = dispatch_one("search");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.contains("usage: search"));
    }

    #[test]
    fn info_by_number_resolves_against_the_search_list() {
        let mut env = FakeEnv::default();
        let mut state = State {
            search_list: vec![li("aur/foo 1-1", "foo"), li("extra/bar 2-1", "bar")],
            ..State::default()
        };
        state.dispatch(&command::parse("info 2"), &mut env);
        assert_eq!(env.info_calls, vec![vec![PkgTarget::new("bar")]]);
    }

    #[test]
    fn info_by_name_passes_through() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("info zlib"), &mut env);
        assert_eq!(env.info_calls, vec![vec![PkgTarget::new("zlib")]]);
    }

    #[test]
    fn info_by_glob_resolves_against_names_universe() {
        let mut env = FakeEnv {
            names: vec!["python-bar".into(), "python-foo".into(), "ruby".into()],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("info python-*"), &mut env);
        assert_eq!(
            env.info_calls,
            vec![vec![
                PkgTarget::new("python-bar"),
                PkgTarget::new("python-foo")
            ]]
        );
    }

    #[test]
    fn info_out_of_range_number_reports_error_without_calling_show() {
        let mut env = FakeEnv::default();
        let mut state = State {
            search_list: vec![li("only 1-1", "only")],
            ..State::default()
        };
        state.dispatch(&command::parse("info 9"), &mut env);
        assert!(env.info_calls.is_empty(), "must not show on a bad index");
        assert!(env.lines.contains("info:"), "got: {:?}", env.lines);
    }

    #[test]
    fn info_with_no_args_prints_usage() {
        let (flow, env) = dispatch_one("info");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.contains("usage: info"));
    }

    fn cart_specs(state: &State) -> Vec<PkgTarget> {
        state
            .cart
            .items()
            .iter()
            .map(|i| PkgTarget::new(i.spec()))
            .collect()
    }

    #[test]
    fn drop_by_repo_filter_unstages_every_aur_row() {
        let mut env = env_with(&[
            ("foo", Source::Aur),
            ("bar", Source::Repo),
            ("baz", Source::Aur),
        ]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar baz"), &mut env);
        state.dispatch(&command::parse("drop aur"), &mut env);
        assert_eq!(cart_specs(&state), vec!["bar"], "`drop aur` drops AUR rows");
    }

    #[test]
    fn drop_by_concrete_repo_filter_targets_one_sync_db() {
        // `upgrade`-seeded rows carry their concrete repo, so a repo-name
        // selector can single out `core` without touching `extra`/`aur`.
        let mut env = FakeEnv {
            upgrade_candidates: vec![
                up("core", "glibc"),
                up("extra", "firefox"),
                up("aur", "yay-bin"),
            ],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env);
        state.dispatch(&command::parse("drop core"), &mut env);
        assert_eq!(cart_specs(&state), vec!["firefox", "yay-bin"]);
    }

    #[test]
    fn add_by_repo_filter_stages_matching_list_rows() {
        let mut env = env_with(&[("firefox", Source::Repo), ("vim", Source::Repo)]);
        env.search_result = vec![
            li_repo("extra", "firefox"),
            li_repo("core", "glibc"),
            li_repo("extra", "vim"),
        ];
        let mut state = State::default();
        // `search` remembers the list `add extra` then filters against.
        state.dispatch(&command::parse("search x"), &mut env);
        state.dispatch(&command::parse("add extra"), &mut env);
        assert_eq!(cart_specs(&state), vec!["firefox", "vim"]);
    }

    #[test]
    fn upgrade_by_repo_filter_seeds_only_that_repo() {
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("core", "glibc"), up("aur", "yay-bin")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade aur"), &mut env);
        assert_eq!(cart_specs(&state), vec!["yay-bin"]);
    }

    #[test]
    fn approve_by_repo_filter_clears_every_aur_row() {
        let mut env = env_with(&[("a", Source::Aur), ("b", Source::Aur), ("c", Source::Repo)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add a b c"), &mut env);
        state.dispatch(&command::parse("approve aur"), &mut env);
        assert!(
            state.cart.all_approved(),
            "`approve aur` clears the AUR gate"
        );
    }

    // --- unified numbering: a bare number follows the last-shown list ---

    #[test]
    fn remove_by_number_on_an_upgrade_row_stages_the_removal_instead() {
        // The reported bug: an upgrade row IS an installed package, but
        // `remove 1` on it used to refuse with "staged for install, not
        // installed" — wrong on both counts, and no path to actually
        // uninstalling it. Removing wins over upgrading: the upgrade row
        // leaves the cart and the removal is staged in its place.
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("aur", "bar"), up("aur", "foo")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env); // view = cart: [bar, foo]
        env.lines.clear();
        state.dispatch(&command::parse("remove 1"), &mut env);
        assert_eq!(state.cart.removals(), &[PkgName::from("bar")]);
        assert_eq!(
            cart_specs(&state),
            vec!["foo"],
            "the converted upgrade row must leave the cart"
        );
        assert!(
            env.lines
                .contains("bar was staged for upgrade — staged removal instead"),
            "should report the conversion: {:?}",
            env.lines
        );
    }

    #[test]
    fn remove_undo_restores_the_converted_upgrade_row() {
        // The upgrade→removal conversion is one cart change: `undo` brings the
        // upgrade row back and unstages the removal.
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("aur", "bar")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env);
        state.dispatch(&command::parse("remove bar"), &mut env);
        state.dispatch(&command::parse("undo"), &mut env);
        assert!(state.cart.removals().is_empty());
        assert_eq!(cart_specs(&state), vec!["bar"]);
    }

    #[test]
    fn remove_rejects_a_staged_fresh_install_and_points_at_drop() {
        // A fresh `add` row (not an upgrade) isn't installed — `remove` on it
        // refuses (you can't uninstall what isn't installed yet) and points at
        // `drop`, which is what "take this cart row out" means.
        let mut env = env_with(&[("bar", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add bar"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("remove bar"), &mut env);
        assert!(
            state.cart.removals().is_empty(),
            "a staged fresh install must not be staged for removal"
        );
        assert_eq!(cart_specs(&state), vec!["bar"], "the install row stays");
        assert!(
            env.lines.any(|l| l.contains("drop bar")),
            "should point at `drop`: {:?}",
            env.lines
        );
    }

    #[test]
    fn remove_by_number_after_search_stages_a_real_uninstall() {
        // `remove` still stages a genuine uninstall when the number lands on a
        // package that isn't a staged install — here a search row.
        let mut env = FakeEnv {
            search_result: vec![li_repo("extra", "oldpkg")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("search x"), &mut env); // view = search
        state.dispatch(&command::parse("remove 1"), &mut env);
        assert_eq!(state.cart.removals(), &[PkgName::from("oldpkg")]);
    }

    #[test]
    fn add_by_number_keeps_pointing_at_the_search_list_across_adds() {
        // Working through a search list: each `add` reprints the cart but must
        // not yank the numbering onto it, so `add 1` then `add 3` both index the
        // search rows (the classic "search, then add a few" flow).
        let mut env = env_with(&[("a", Source::Aur), ("b", Source::Aur), ("c", Source::Aur)]);
        env.search_result = vec![
            li_repo("aur", "a"),
            li_repo("aur", "b"),
            li_repo("aur", "c"),
        ];
        let mut state = State::default();
        state.dispatch(&command::parse("search x"), &mut env);
        state.dispatch(&command::parse("add 1"), &mut env);
        state.dispatch(&command::parse("add 3"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["a", "c"],
            "1 and 3 index the search list, not the reprinted cart"
        );
    }

    #[test]
    fn number_indexes_the_cart_when_no_search_was_run() {
        // Fresh session, staged straight into the cart (no `search`): a bare
        // number must still resolve against the cart on screen, not error with
        // "no numbered list is up".
        let mut env = env_with(&[("foo", Source::Aur), ("bar", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar"), &mut env); // cart: [bar, foo]
        state.dispatch(&command::parse("drop 1"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["foo"],
            "`drop 1` hits the shown cart"
        );
    }

    #[test]
    fn drop_by_number_indexes_the_shown_cart() {
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("aur", "bar"), up("aur", "foo")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env); // cart: [bar, foo]
        state.dispatch(&command::parse("drop 1"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["foo"],
            "`drop 1` drops shown row 1"
        );
    }

    #[test]
    fn show_switches_numbering_from_the_search_list_to_the_cart() {
        let mut env = env_with(&[("staged", Source::Aur)]);
        env.search_result = vec![li_repo("aur", "searched")];
        let mut state = State::default();
        state.dispatch(&command::parse("add staged"), &mut env); // cart = [staged]
        state.dispatch(&command::parse("search x"), &mut env); // view = search
        state.dispatch(&command::parse("info 1"), &mut env);
        assert_eq!(
            env.info_calls.last(),
            Some(&vec![PkgTarget::new("searched")]),
            "in the search view, `1` is the search row"
        );
        state.dispatch(&command::parse("show"), &mut env); // view = cart
        state.dispatch(&command::parse("info 1"), &mut env);
        assert_eq!(
            env.info_calls.last(),
            Some(&vec![PkgTarget::new("staged")]),
            "after `show`, `1` is the cart row"
        );
    }

    // --- undo / redo ---

    #[test]
    fn undo_restores_a_cart_over_narrowed_by_keep() {
        // The reported bug: `keep` dropped more than intended and the rows were
        // gone for good. `undo` brings the whole pre-`keep` cart back.
        let mut env = env_with(&[
            ("foo", Source::Aur),
            ("bar", Source::Aur),
            ("baz", Source::Aur),
        ]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar baz"), &mut env);
        state.dispatch(&command::parse("keep bar"), &mut env);
        assert_eq!(cart_specs(&state), vec!["bar"]);
        state.dispatch(&command::parse("undo"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["bar", "baz", "foo"],
            "undo restores every row `keep` dropped"
        );
    }

    #[test]
    fn redo_reapplies_an_undone_change() {
        let mut env = env_with(&[
            ("foo", Source::Aur),
            ("bar", Source::Aur),
            ("baz", Source::Aur),
        ]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar baz"), &mut env);
        state.dispatch(&command::parse("keep bar"), &mut env);
        state.dispatch(&command::parse("undo"), &mut env);
        state.dispatch(&command::parse("redo"), &mut env);
        assert_eq!(cart_specs(&state), vec!["bar"], "redo reapplies the keep");
    }

    #[test]
    fn undo_steps_back_one_change_at_a_time() {
        let mut env = env_with(&[("foo", Source::Aur), ("bar", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        state.dispatch(&command::parse("add bar"), &mut env);
        assert_eq!(cart_specs(&state), vec!["bar", "foo"]);
        state.dispatch(&command::parse("undo"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["foo"],
            "one undo reverts `add bar`"
        );
        state.dispatch(&command::parse("undo"), &mut env);
        assert!(state.cart.is_empty(), "the next undo reverts `add foo`");
    }

    #[test]
    fn a_fresh_change_forgets_the_redo_branch() {
        let mut env = env_with(&[
            ("foo", Source::Aur),
            ("bar", Source::Aur),
            ("qux", Source::Aur),
        ]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar"), &mut env);
        state.dispatch(&command::parse("drop foo"), &mut env);
        state.dispatch(&command::parse("undo"), &mut env); // redo branch now holds the post-drop cart
        state.dispatch(&command::parse("add qux"), &mut env); // a new edit forks the branch
        env.lines.clear();
        state.dispatch(&command::parse("redo"), &mut env);
        assert!(
            env.lines.contains("nothing to redo"),
            "the redo branch was discarded by the new edit: {:?}",
            env.lines
        );
    }

    #[test]
    fn undo_with_no_history_is_a_friendly_no_op() {
        let (_flow, env) = dispatch_one("undo");
        assert!(env.lines.contains("nothing to undo"));
    }

    #[test]
    fn redo_with_no_undone_change_is_a_friendly_no_op() {
        let (_flow, env) = dispatch_one("redo");
        assert!(env.lines.contains("nothing to redo"));
    }

    #[test]
    fn clear_is_undoable() {
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        state.dispatch(&command::parse("clear"), &mut env);
        assert!(state.cart.is_empty());
        state.dispatch(&command::parse("undo"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["foo"],
            "undo brings back a cleared cart"
        );
    }

    #[test]
    fn a_clean_apply_forgets_the_undo_history() {
        // After a transaction runs, the snapshots describe an old world (they'd
        // re-stage now-installed packages), so `apply` drops them.
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        state.dispatch(&command::parse("approve foo"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env); // FakeEnv default: Succeeded
        assert!(state.cart.is_empty(), "a clean apply empties the cart");
        env.lines.clear();
        state.dispatch(&command::parse("undo"), &mut env);
        assert!(
            env.lines.contains("nothing to undo"),
            "apply cleared the undo history: {:?}",
            env.lines
        );
    }

    #[test]
    fn expand_repo_tokens_expands_known_repos_and_passes_others_through() {
        let rows = vec![
            RepoRow {
                target: PkgTarget::new("glibc"),
                repo: Some(RepoName::from("core")),
            },
            RepoRow {
                target: PkgTarget::new("yay-bin"),
                repo: Some(RepoName::from("aur")),
            },
        ];
        // A repo name expands to its rows' targets…
        assert_eq!(expand_repo_tokens(&[s("aur")], &rows), vec!["yay-bin"]);
        // …case-insensitively…
        assert_eq!(expand_repo_tokens(&[s("CORE")], &rows), vec!["glibc"]);
        // …while numbers, names, and globs pass through untouched.
        assert_eq!(expand_repo_tokens(&[s("3")], &rows), vec!["3"]);
        assert_eq!(expand_repo_tokens(&[s("nginx")], &rows), vec!["nginx"]);
        assert_eq!(expand_repo_tokens(&[s("py-*")], &rows), vec!["py-*"]);
    }

    fn s(t: &str) -> String {
        t.to_owned()
    }
}
