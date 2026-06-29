//! Interactive shell (REPL) for the no-arg `gaur` invocation.
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

use crate::build::{self, ConfirmGate, InstallOpts, UpgradeSession, review};
use crate::cli::dispatch;
use crate::cli::search::Row;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::index::{self, IndexEntry};
use crate::mirror::{self, MirrorRepo};
use crate::names::{PkgBase, PkgName, PkgTarget, RepoName, SearchTerm};
use crate::pacman::alpm_db::{self, PacmanIndex};
use crate::pacman::invoke::{self, PkgUpgrade, REPO_AUR};
use crate::paths;
use crate::resolver::Plan;
use crate::ui::{self, UpgradeSelection};
use crate::version::Version;
use cart::{
    ApplyOutcome, Approval, ApproveResult, AurApproval, Cart, CartItem, ReviewOutcome, Source,
    StageClass, StageResult, UnstageResult,
};
use command::Command;
use complete::ShellHelper;
use rustyline::Editor;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, instrument};

pub mod cart;
pub mod command;
pub mod complete;
pub mod selector;
pub mod upgrade;

/// One row of the most recent result list, addressable by its 1-based number.
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

/// Mutable per-session shell state the dispatch core threads between commands.
#[derive(Default)]
pub struct State {
    /// The last printed result list (`search`), addressable by number.
    last_list: Vec<ListItem>,
    /// The staged transaction `apply` runs.
    cart: Cart,
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
    /// Whether AUR items stage pre-approved (config `review_default == "skip"`).
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
            Command::Help(_topic) => {
                env.print(HELP_TEXT);
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
                self.show(env);
                Flow::Continue
            }
            Command::Apply => {
                self.apply(env);
                Flow::Continue
            }
            Command::Clear => {
                if self.cart.is_empty() {
                    env.print("cart is already empty");
                } else {
                    self.cart.clear();
                    env.print("cart cleared");
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
                    for (i, item) in items.iter().enumerate() {
                        env.print(&format!("{:>3}  {}", i + 1, item.label));
                    }
                }
                // Replace the current list even when empty, so a stale list
                // can't be addressed by number after a fruitless search.
                self.last_list = items;
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
        let mut staged = 0;
        for u in to_seed {
            if self.cart.add(CartItem::from_upgrade(u, policy)) == StageResult::Staged {
                staged += 1;
            }
        }
        env.print(&format!("{staged} upgrade(s) staged"));
        self.show(env);
    }

    /// `add <sel…>`: classify each selected target and stage it. Selectors
    /// resolve against the last search list (numbers) + the full name universe
    /// (names/globs), so you can `add` anything installable.
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
                        StageResult::Staged => env.print(&format!("staged {name} ({label})")),
                        StageResult::AlreadyStaged => {
                            env.print(&format!("{name} is already staged"));
                        }
                    }
                }
                None => env.print(&format!("unknown package `{}` — not staged", t.as_str())),
            }
        }
    }

    /// `drop <sel…>`: unstage installs from the cart. Selectors resolve against
    /// the cart (numbers index the staged rows; names/globs match staged specs).
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
        for t in targets {
            match self.cart.unstage(&t) {
                UnstageResult::Unstaged => env.print(&format!("dropped {}", t.as_str())),
                UnstageResult::NotStaged => env.print(&format!("{} wasn't staged", t.as_str())),
            }
        }
    }

    /// `remove <sel…>`: stage an uninstall (`pacman -R` at apply). Selectors
    /// resolve against the last list + universe; pacman validates names at
    /// apply time.
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
        for t in targets {
            let name = PkgName::from(t.into_inner());
            match self.cart.stage_remove(name.clone()) {
                StageResult::Staged => env.print(&format!("staged removal of {name}")),
                StageResult::AlreadyStaged => {
                    env.print(&format!("{name} is already staged for removal"));
                }
            }
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
        for t in targets {
            match self.cart.approve(&t) {
                ApproveResult::Approved => {
                    if let Some(pb) = env.pkgbase_of(&t) {
                        self.cart.mark_reviewed(pb);
                    }
                    env.print(&format!("approved {}", t.as_str()));
                }
                ApproveResult::AlreadyApproved => {
                    env.print(&format!("{} is already approved", t.as_str()));
                }
                ApproveResult::NotStaged => {
                    env.print(&format!("{} isn't staged", t.as_str()));
                }
            }
        }
    }

    /// `review <sel…>`: open each selected AUR item's PKGBUILD (diff-against-
    /// installed or full) and approve/skip per the user's call. Repo items have
    /// no PKGBUILD; already-approved items are left alone; an abort stops the
    /// pass.
    fn review<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
        if args.is_empty() {
            env.print("usage: review <pkg|number|range|glob>…");
            return;
        }
        let targets = match self.resolve_against_cart(args) {
            Ok(t) => t,
            Err(e) => {
                env.print(&format!("review: {e}"));
                return;
            }
        };
        if targets.is_empty() {
            env.print("review: nothing in the cart matched");
            return;
        }
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
                Some((Source::Aur, Approval::NeedsReview)) => match env.review(&t) {
                    Ok(ReviewOutcome::Approved) => {
                        self.cart.approve(&t);
                        if let Some(pb) = env.pkgbase_of(&t) {
                            self.cart.mark_reviewed(pb);
                        }
                        env.print(&format!("approved {}", t.as_str()));
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
                },
            }
        }
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
                env.print("done");
            }
            Ok(ApplyOutcome::Failed) => {
                env.print("some packages didn't apply — `drop` them and `apply` again");
            }
            Err(e) => env.print(&format!("apply: {e}")),
        }
    }

    /// Resolve selector `args` against the cart: a repo name (`aur`, `core`, …)
    /// selects every staged row from that repo; numbers index the staged rows;
    /// names/globs match staged specs. Mirrors the verb-scoping rule in the
    /// design (cart verbs act on what's staged, not the search list).
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
        let list: Vec<ListItem> = rows
            .iter()
            .map(|r| ListItem {
                target: r.target.clone(),
                label: String::new(),
                repo: r.repo.clone(),
            })
            .collect();
        let universe: Vec<PkgTarget> = rows.iter().map(|r| r.target.clone()).collect();
        selector::resolve(&args, &list, &universe)
    }

    /// Resolve selector `args` against the last result list: a repo name selects
    /// every list row from that repo; numbers/ranges index the list; names/globs
    /// resolve against the full name universe. The list verbs (`add`, `info`,
    /// `remove`) share this so `add extra` and `add 3` behave the same way.
    fn resolve_against_list<E: ShellEnv>(
        &self,
        args: &[String],
        env: &E,
    ) -> std::result::Result<Vec<PkgTarget>, String> {
        let rows: Vec<RepoRow> = self
            .last_list
            .iter()
            .map(|it| RepoRow {
                target: it.target.clone(),
                repo: it.repo.clone(),
            })
            .collect();
        let args = expand_repo_tokens(args, &rows);
        selector::resolve(&args, &self.last_list, env.names())
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
  remove <sel…>       stage packages to uninstall
  upgrade [pkg…]      upgrade installed packages (repo + AUR)
  review <sel…>       view a PKGBUILD/diff and approve it
  approve <sel…>      approve staged AUR packages without a diff (try `approve *`)
  show                preview the staged transaction
  apply               build + install the staged transaction
  clear               empty the cart
  refresh             re-fetch the AUR mirror + index
  help                this list
  quit                leave the shell (also: Ctrl-D)
selectors: `3` (row), `5-8` (range), `glibc` (name), `python-*` (glob),
           `aur`/`core`/… (whole repo — e.g. `drop aur`, `add extra`)";

/// Run the interactive shell. Returns the desired process exit code.
#[instrument(skip(cfg))]
pub fn run(cfg: &Config, devel: bool) -> Result<u8> {
    info!(devel, "shell session start");
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
    };
    let mut state = State::default();

    env.print("gitaur shell — type `help` for commands, `quit` to leave");
    if env.session.is_none() {
        env.print("no AUR index yet — run `gaur -Sy` to enable AUR search/info");
    }

    let helper = ShellHelper::new(Rc::clone(&env.caches.universe));
    let mut rl: Editor<ShellHelper, DefaultHistory> =
        Editor::new().map_err(|e| Error::other(format!("shell: init line editor: {e}")))?;
    rl.set_helper(Some(helper));
    let history = paths::shell_history_path();
    // A missing history file on first run is expected, not an error.
    rl.load_history(&history).ok();

    let code = loop {
        match rl.readline("gaur> ") {
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
    devel: bool,
    session: Option<UpgradeSession>,
    caches: NameCaches,
}

impl ShellEnv for RealEnv<'_> {
    fn print(&mut self, line: &str) {
        println!("{line}");
    }

    fn upgrade(&mut self) -> Result<Vec<PkgUpgrade>> {
        self.reload()?;
        match &self.session {
            Some(session) => session.recompute_remaining(self.devel),
            // No AUR index even after a refresh: repo upgrades are still
            // queryable straight from the synced db.
            None => invoke::query_repo_upgrades(),
        }
    }

    fn refresh(&mut self) -> Result<()> {
        self.reload()
    }

    fn search(&mut self, terms: &[SearchTerm]) -> Result<Vec<ListItem>> {
        let regexes: Vec<regex::Regex> = terms
            .iter()
            .map(SearchTerm::compile)
            .collect::<std::result::Result<_, _>>()?;
        let color = ui::color_on();
        // Repo hits first (yay/paru "official repos on top"); they need no index.
        let mut rows: Vec<Row<'_>> = alpm_db::search_sync(terms)?
            .into_iter()
            .map(Row::Repo)
            .collect();
        if let Some(session) = &self.session {
            let mut aur = session.secondary().search(session.index(), &regexes);
            // Freshest commit first, pkgbase tie-break — same order as `-Ss`.
            aur.sort_by(|a, b| {
                b.commit_time_unix
                    .cmp(&a.commit_time_unix)
                    .then_with(|| a.pkgbase.cmp(&b.pkgbase))
            });
            rows.extend(aur.into_iter().map(Row::Aur));
        }
        Ok(rows
            .iter()
            .map(|r| ListItem {
                target: r.picked(),
                label: r.label(color),
                repo: Some(RepoName::from(r.repo_name())),
            })
            .collect())
    }

    fn show_info(&mut self, targets: &[PkgTarget]) -> Result<()> {
        let Some(session) = &self.session else {
            ui::warn("no AUR index; run `gaur -Sy` first");
            return Ok(());
        };
        // `info_targets` already warns about misses; the shell doesn't propagate
        // per-command exit codes, so the returned missing-list is discarded.
        index::info_targets(session.index(), session.secondary(), targets);
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
        // The one place config `review_default` finally drives behaviour:
        // `skip` ⇒ AUR stages pre-approved, everything else ⇒ needs review.
        if self.cfg.review_default == "skip" {
            AurApproval::Auto
        } else {
            AurApproval::Review
        }
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
            false,
        ) {
            Ok(review::Outcome::Approved) => Ok(ReviewOutcome::Approved),
            Ok(review::Outcome::Skipped) => Ok(ReviewOutcome::Skipped),
            // An abort at the review prompt ends the pass but not the shell.
            Err(Error::UserAbort) => Ok(ReviewOutcome::Aborted),
            Err(e) => Err(e),
        }
    }

    fn render_cart(&mut self, cart: &Cart) {
        // Resolve the staged set into the full change set (roots + pulled-in
        // deps + removals + cost) and render the one unified table. `show` must
        // never error out, so a resolve failure degrades to the flat staged
        // rows plus a note (UPDATE_LOOP goal #5 landing behind `show`).
        match self.transaction_view(cart) {
            Ok(table) => {
                for line in table.lines() {
                    self.print(line);
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
        let Some(session) = self.session.as_ref() else {
            ui::warn("no AUR index loaded; cannot apply");
            return Ok(ApplyOutcome::Failed);
        };
        let pac = upgrade::system_pac()?;

        // Resolve the build/install half (AUR + fresh installs); repo *upgrades*
        // take the partial `-Syu` lane below, so they're excluded from the plan.
        let plan = self.resolve_cart(session, &pac, cart)?;

        // No table redraw — `show` is where the user looked. `apply` gates on the
        // one-line cost summary plus a single confirm (phase 5a).
        let size_pac = upgrade::synced_pac()?;
        let roots = txn_roots(cart, session, &size_pac);
        let (repo_deps, aur_deps) = upgrade::dep_rows(plan.as_ref());
        let removals: Vec<PkgName> = cart.removals().to_vec();
        let metrics = upgrade::preview_metrics(session, &roots, plan.as_ref());
        ui::info(&ui::cost_summary(
            &roots, &repo_deps, &aur_deps, &removals, &size_pac, &metrics,
        ));
        if !ui::confirm("Proceed with this transaction?", false)
            .map_err(|e| Error::other(format!("confirm: {e}")))?
        {
            return Ok(ApplyOutcome::Declined);
        }

        // Repo upgrades first (so AUR builds link against the upgraded libs), via
        // a partial `pacman -Syu` that ignores every repo candidate the user
        // didn't stage.
        let repo_sel = self.repo_upgrade_selection(session, cart)?;
        if !repo_sel.repo.is_empty() {
            dispatch::run_repo_upgrade(self.cfg, &repo_sel)?;
        }

        // Build + install the AUR (and any fresh-install) half. Already gated by
        // the confirm above, so `apply_plan` doesn't re-ask.
        let mut reviewed = cart.reviewed().clone();
        let opts = InstallOpts {
            noconfirm: false,
            asdeps: false,
            gate: ConfirmGate::AlreadyConfirmed,
        };
        let outcome = match &plan {
            Some(plan) => {
                let report =
                    build::apply_plan(self.cfg, session.index(), &pac, plan, opts, &mut reviewed)?;
                outcome_of(&report)
            }
            None => ApplyOutcome::Succeeded,
        };
        if outcome != ApplyOutcome::Succeeded {
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
                return Ok(ApplyOutcome::Failed);
            }
        }
        Ok(ApplyOutcome::Succeeded)
    }
}

impl RealEnv<'_> {
    /// Re-fetch the mirror + index and reload the session in place, rebuilding
    /// the name caches so fresh data backs subsequent `search`/`info`/
    /// classification + completion. Shared by `upgrade` (which then recomputes
    /// candidates) and `refresh` (which stops here).
    fn reload(&mut self) -> Result<()> {
        let session = upgrade::refresh_and_reload(self.cfg)?;
        self.caches = build_universe(session.as_ref());
        self.session = session;
        Ok(())
    }

    /// Build the unified change-set table for `show`: resolve the staged set,
    /// collect the pulled-in deps, sizes, and build-time overlay, and hand them
    /// to [`ui::transaction_table`]. Errors bubble up so `render_cart` can fall
    /// back to the flat rows — `show` must never abort.
    fn transaction_view(&self, cart: &Cart) -> Result<ui::Table> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::other("no AUR index loaded"))?;
        let pac = upgrade::system_pac()?;
        let plan = self.resolve_cart(session, &pac, cart)?;
        // Sizes from the freshly-synced db (the new versions' real download cost).
        let size_pac = upgrade::synced_pac()?;
        let roots = txn_roots(cart, session, &size_pac);
        let (repo_deps, aur_deps) = upgrade::dep_rows(plan.as_ref());
        let removals: Vec<PkgName> = cart.removals().to_vec();
        let metrics = upgrade::preview_metrics(session, &roots, plan.as_ref());
        Ok(ui::transaction_table(
            &roots, &repo_deps, &aur_deps, &removals, &size_pac, &metrics,
        ))
    }

    /// Resolve the cart's install/build half into a [`Plan`] — the AUR rows and
    /// fresh installs (repo *upgrades* take the `-Syu` lane, so they aren't
    /// targets here). `None` when nothing needs the build pipeline (a
    /// repo-upgrade-only or removal-only cart).
    fn resolve_cart(
        &self,
        session: &UpgradeSession,
        pac: &PacmanIndex,
        cart: &Cart,
    ) -> Result<Option<Plan>> {
        let targets = cart.install_targets();
        if targets.is_empty() {
            return Ok(None);
        }
        let plan = build::resolve_targets(
            self.cfg,
            session.index(),
            Some(session.secondary()),
            pac,
            &targets,
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

/// Map a build [`RunReport`](build::RunReport) to the cart-apply outcome: any
/// failure, dep-skip, or interrupt keeps the cart for a retry.
fn outcome_of(report: &build::RunReport) -> ApplyOutcome {
    if report.failed.is_empty() && report.skipped_dep.is_empty() && report.interrupted.is_empty() {
        ApplyOutcome::Succeeded
    } else {
        ApplyOutcome::Failed
    }
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
/// index entry, or when the commit time is unrecorded (`0` in
/// pre-`commit_time_unix` archives).
fn aur_age(it: &CartItem, session: &UpgradeSession, now: SystemTime) -> Option<Duration> {
    if it.source != Source::Aur {
        return None;
    }
    let entry = session.secondary().lookup(session.index(), it.spec())?;
    let secs = u64::try_from(entry.commit_time_unix)
        .ok()
        .filter(|&s| s > 0)?;
    let modified = UNIX_EPOCH + Duration::from_secs(secs);
    now.duration_since(modified).ok()
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
    fn apply_failure_keeps_the_cart_for_retry() {
        let mut env = env_with(&[("glibc", Source::Repo)]);
        env.apply_outcome = Some(ApplyOutcome::Failed);
        let mut state = State::default();
        state.dispatch(&command::parse("add glibc"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env);
        assert_eq!(state.cart.items().len(), 1, "failed apply keeps the cart");
        assert!(env.lines.contains("didn't apply"));
    }

    #[test]
    fn remove_stages_an_uninstall() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("remove oldpkg"), &mut env);
        assert_eq!(state.cart.removals(), &[PkgName::from("oldpkg")]);
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
        assert_eq!(state.last_list.len(), 2, "the list should be remembered");
    }

    #[test]
    fn search_with_no_terms_prints_usage() {
        let (flow, env) = dispatch_one("search");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.contains("usage: search"));
    }

    #[test]
    fn info_by_number_resolves_against_the_last_list() {
        let mut env = FakeEnv::default();
        let mut state = State {
            last_list: vec![li("aur/foo 1-1", "foo"), li("extra/bar 2-1", "bar")],
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
            last_list: vec![li("only 1-1", "only")],
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
