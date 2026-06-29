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
use crate::index;
use crate::mirror::{self, MirrorRepo};
use crate::names::{PkgBase, PkgName, PkgTarget, RepoName, SearchTerm};
use crate::pacman::alpm_db::{self, PacmanIndex};
use crate::pacman::invoke::{self, PkgUpgrade, REPO_AUR};
use crate::paths;
use crate::ui::{self, UpgradeSelection};
use cart::{
    ApplyOutcome, Approval, ApproveResult, AurApproval, Cart, CartItem, ReviewOutcome, Source,
    StageClass,
};
use command::Command;
use console::style;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, info, instrument};

pub mod cart;
pub mod command;
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
                // The mid-session re-fetch lands in phase 5; until then a fresh
                // upstream snapshot is what restarting `gaur` is for.
                env.print(
                    "refresh isn't wired up yet (phase 5) — restart `gaur` to re-fetch the mirror",
                );
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
            if self.cart.add(CartItem::from_upgrade(u, policy)) {
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
                    if self.cart.add(CartItem::new(t, source, repo, policy)) {
                        env.print(&format!("staged {name} ({label})"));
                    } else {
                        env.print(&format!("{name} is already staged"));
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
            if self.cart.unstage(&t) {
                env.print(&format!("dropped {}", t.as_str()));
            } else {
                env.print(&format!("{} wasn't staged", t.as_str()));
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
            if self.cart.stage_remove(name.clone()) {
                env.print(&format!("staged removal of {name}"));
            } else {
                env.print(&format!("{name} is already staged for removal"));
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
           `aur`/`core`/… (whole repo — e.g. `drop aur`, `add extra`)
note: `refresh` lands in a later phase.";

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

    let mut rl =
        DefaultEditor::new().map_err(|e| Error::other(format!("shell: init line editor: {e}")))?;
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
                if let Flow::Exit(code) = state.dispatch(&command::parse(&line), &mut env) {
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
    /// Backs glob resolution and, later, tab-completion.
    universe: Vec<PkgTarget>,
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
    NameCaches { universe, sync }
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
        // Refresh the mirror + index and reload the session in place, so the
        // fresh data backs subsequent `search`/`info`/classification too.
        let session = upgrade::refresh_and_reload(self.cfg)?;
        self.caches = build_universe(session.as_ref());
        self.session = session;
        match &self.session {
            Some(session) => session.recompute_remaining(self.devel),
            // No AUR index even after a refresh: repo upgrades are still
            // queryable straight from the synced db.
            None => invoke::query_repo_upgrades(),
        }
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
        let colored = Colored::detect();
        let items = cart.items();
        let now = SystemTime::now();

        // Column widths over the plain (uncolored) cell text — padding is then
        // applied on that plain width so embedded ANSI codes don't skew columns.
        let repo_w = Width::widest(items.iter().map(|i| Width::of(i.repo_label().as_str())));
        let appr_w = Width::widest(items.iter().map(|i| Width::of(i.approval.label())));
        let name_w = Width::widest(items.iter().map(|i| Width::of(i.spec())));
        let old_w = Width::widest(
            items
                .iter()
                .filter_map(|i| i.upgrade.as_ref())
                .map(|u| Width::of(&u.old_ver.to_string())),
        );
        let new_w = Width::widest(
            items
                .iter()
                .filter_map(|i| i.upgrade.as_ref())
                .map(|u| Width::of(&u.new_ver.to_string())),
        );
        // The fixed-width version block (`old → new`), so a fresh-install row's
        // blank keeps the trailing age column aligned with the upgrade rows.
        let ver_w = if old_w.is_zero() && new_w.is_zero() {
            Width::ZERO
        } else {
            old_w + new_w + ARROW_PAD
        };

        for (i, it) in items.iter().enumerate() {
            let repo = it.repo_label();
            let repo_cell =
                repo_w.pad_left(colored.paint(repo.as_str(), |s| ui::repo(s).to_string()));

            let appr = it.approval.label();
            let appr_cell = appr_w.pad_left(colored.paint(appr, |s| match it.approval {
                Approval::Approved => style(s).green().to_string(),
                Approval::NeedsReview => style(s).yellow().to_string(),
            }));

            let name = it.spec();
            let name_cell = name_w.pad_left(colored.paint(name, |s| style(s).bold().to_string()));

            let ver_cell = version_cell(it, old_w, new_w, ver_w, colored);
            // "Last modified" age, AUR rows only (yay-style staleness hint).
            let age_cell = match (it.source, self.aur_last_modified(it)) {
                (Source::Aur, Some(modified)) => {
                    let age = now.duration_since(modified).unwrap_or_default();
                    let label = format!("({} ago)", ui::human_age(age));
                    format!(
                        "  {}",
                        colored.paint(&label, |s| ui::dim(s).to_string()).rendered
                    )
                }
                _ => String::new(),
            };

            self.print(&format!(
                "{:>3}  {repo_cell}  {appr_cell}  {name_cell}{ver_cell}{age_cell}",
                i + 1
            ));
        }

        for name in cart.removals() {
            let tag = colored
                .paint("remove", |s| style(s).red().to_string())
                .rendered;
            self.print(&format!("     {tag}  {name}"));
        }
    }

    fn apply(&mut self, cart: &Cart) -> Result<ApplyOutcome> {
        let Some(session) = &self.session else {
            ui::warn("no AUR index loaded; cannot apply");
            return Ok(ApplyOutcome::Failed);
        };
        let pac = upgrade::system_pac()?;

        // The upgrade rows (repo + AUR) drive the cost-overlay preview and the
        // partial `-Syu` lane; their absence means a pure fresh-install cart.
        let roots: Vec<PkgUpgrade> = cart
            .items()
            .iter()
            .filter_map(|i| i.upgrade.clone())
            .collect();

        let outcome = if roots.is_empty() {
            self.apply_installs(session, &pac, cart)?
        } else {
            self.apply_upgrades(session, &pac, cart, &roots)?
        };
        if outcome != ApplyOutcome::Succeeded {
            return Ok(outcome);
        }

        // Remove half (shared): `pacman -R`, filtered to packages actually
        // installed so a retry after a partial failure doesn't trip on an
        // already-gone target. One atomic add+remove transaction is the phase-6
        // native-commit goal; until then this is separate transactions bridged
        // by the sudo cache.
        let removals: Vec<&PkgName> = cart
            .removals()
            .iter()
            .filter(|n| pac.is_installed(n.as_str()))
            .collect();
        if !removals.is_empty() {
            // Stringify only here, at pacman's argv boundary.
            let mut args = vec!["-R".to_owned()];
            args.extend(removals.iter().map(|n| n.as_str().to_owned()));
            if invoke::exec_pacman(self.cfg, &args)? != 0 {
                ui::warn("removal step did not complete");
                return Ok(ApplyOutcome::Failed);
            }
        }
        Ok(ApplyOutcome::Succeeded)
    }
}

impl RealEnv<'_> {
    /// When the AUR pkgbase a staged row resolves to was last modified (its
    /// branch-tip commit time), for the `show` table's age column. `None` when
    /// there's no index, the row isn't an AUR package, or the timestamp is
    /// unrecorded (`0` in pre-`commit_time_unix` index archives).
    fn aur_last_modified(&self, item: &CartItem) -> Option<SystemTime> {
        let session = self.session.as_ref()?;
        let entry = session.secondary().lookup(session.index(), item.spec())?;
        let secs = u64::try_from(entry.commit_time_unix)
            .ok()
            .filter(|&s| s > 0)?;
        Some(UNIX_EPOCH + Duration::from_secs(secs))
    }

    /// Pure fresh-install cart (no upgrade rows): the `-S` pipeline owns the
    /// plan table + its only-when-deps confirm and the stratified build/install.
    /// The cart's reviewed set rides along so approved AUR pkgbases aren't
    /// re-prompted.
    fn apply_installs(
        &self,
        session: &UpgradeSession,
        pac: &PacmanIndex,
        cart: &Cart,
    ) -> Result<ApplyOutcome> {
        let targets = cart.install_targets();
        if targets.is_empty() {
            return Ok(ApplyOutcome::Succeeded);
        }
        let mut reviewed = cart.reviewed().clone();
        let opts = InstallOpts {
            noconfirm: false,
            asdeps: false,
            gate: ConfirmGate::Ask,
        };
        match build::install_with_index(
            self.cfg,
            session.index(),
            Some(session.secondary()),
            pac,
            &targets,
            opts,
            &mut reviewed,
        ) {
            Ok(report) => Ok(outcome_of(&report)),
            // Declining the plan confirm aborts the whole apply, cart kept.
            Err(Error::UserAbort) => Ok(ApplyOutcome::Declined),
            Err(e) => Err(e),
        }
    }

    /// Upgrade cart: resolve the AUR/build half once, render the cost-overlay
    /// change-set preview, take one confirm, then run the partial `-Syu` repo
    /// lane (so AUR builds link against the upgraded libs) and the build/install
    /// lane.
    fn apply_upgrades(
        &self,
        session: &UpgradeSession,
        pac: &PacmanIndex,
        cart: &Cart,
        roots: &[PkgUpgrade],
    ) -> Result<ApplyOutcome> {
        // Resolve the AUR upgrades + any fresh installs once; repo *upgrades*
        // are excluded (they take the `-Syu` lane below).
        let targets = cart.install_targets();
        let plan = build::resolve_targets(
            self.cfg,
            session.index(),
            Some(session.secondary()),
            pac,
            &targets,
            false,
        )?;

        // Sizes from the freshly-synced db (the new versions' real download
        // cost); build-time from the metrics store.
        let size_pac = upgrade::synced_pac()?;
        let metrics = upgrade::preview_metrics(session, roots, Some(&plan));
        upgrade::preview(roots, Some(&plan), &size_pac, &metrics);
        if !ui::confirm("Proceed with this transaction?", false)
            .map_err(|e| Error::other(format!("confirm: {e}")))?
        {
            return Ok(ApplyOutcome::Declined);
        }

        // Repo upgrades first, via a partial `pacman -Syu` that ignores every
        // repo candidate the user didn't stage.
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
        let report = build::apply_plan(self.cfg, session.index(), pac, &plan, opts, &mut reviewed)?;
        Ok(outcome_of(&report))
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

/// The ` → ` separator between the old and new version in an upgrade row.
const ARROW: &str = " → ";
/// Display width of [`ARROW`] (the arrow glyph is one column, flanked by two
/// spaces). Separate from `ARROW.len()` (5 bytes) so the version-block width
/// math and the rendered separator can't drift.
const ARROW_PAD: Width = Width(3);

/// A terminal column width in display cells, used to align the `show` table.
///
/// A newtype (not a bare `usize`) so a width can't be confused with an index or
/// a count, and so the pad-on-visible-width policy lives in one place — see
/// [`Self::pad_left`].
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
struct Width(usize);

impl Width {
    const ZERO: Self = Self(0);

    /// Visible width of a plain (un-colored) cell. Cells are ASCII (names,
    /// versions, repo labels), so char count equals display columns.
    fn of(s: &str) -> Self {
        Self(s.chars().count())
    }

    /// The widest of a set of cell widths — the column width. `ZERO` if empty.
    fn widest(widths: impl Iterator<Item = Self>) -> Self {
        widths.max().unwrap_or(Self::ZERO)
    }

    fn is_zero(self) -> bool {
        self == Self::ZERO
    }

    /// `self` spaces — the blank for a fixed-width column with no content.
    fn blanks(self) -> String {
        " ".repeat(self.0)
    }

    /// The padding needed to widen a cell of width `inner` to `self`.
    const fn gap(self, inner: Self) -> Self {
        Self(self.0.saturating_sub(inner.0))
    }

    /// Left-justify `cell` to `self` (rendered text, then trailing spaces).
    fn pad_left(self, cell: Cell) -> String {
        let Cell { rendered, width } = cell;
        format!("{rendered}{}", self.gap(width).blanks())
    }

    /// Right-justify `cell` to `self` (leading spaces, then rendered text).
    fn pad_right(self, cell: Cell) -> String {
        let Cell { rendered, width } = cell;
        format!("{}{rendered}", self.gap(width).blanks())
    }
}

impl std::ops::Add for Width {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

/// A rendered table cell: the (possibly colored) display string plus the width
/// of its *plain* text, so [`Width::pad_left`] aligns on visible width even when
/// `rendered` carries ANSI escapes.
struct Cell {
    rendered: String,
    width: Width,
}

/// Whether the table renders with ANSI color — a two-state enum rather than a
/// bare `bool`, so `Colored::No` reads as intent and the color branch lives in
/// one place ([`Self::paint`]).
#[derive(Clone, Copy)]
enum Colored {
    Yes,
    No,
}

impl Colored {
    /// Detect from the global color mode ([`ui::color_on`]).
    fn detect() -> Self {
        if ui::color_on() { Self::Yes } else { Self::No }
    }

    /// Render `plain`, applying `paint` only when color is enabled. The returned
    /// [`Cell`] remembers `plain`'s visible width for later alignment.
    fn paint(self, plain: &str, paint: impl FnOnce(&str) -> String) -> Cell {
        Cell {
            width: Width::of(plain),
            rendered: match self {
                Self::Yes => paint(plain),
                Self::No => plain.to_owned(),
            },
        }
    }
}

/// The version cell for one cart row: `old → new` for an upgrade, a blank of the
/// same width for a fresh install (so the trailing age column stays aligned),
/// or empty when the cart has no upgrade rows at all (`ver_w` is zero). Includes
/// its own leading column separator.
fn version_cell(
    it: &CartItem,
    old_w: Width,
    new_w: Width,
    ver_w: Width,
    colored: Colored,
) -> String {
    if ver_w.is_zero() {
        return String::new();
    }
    let Some(u) = it.upgrade.as_ref() else {
        return format!("  {}", ver_w.blanks());
    };
    let old = u.old_ver.to_string();
    let new = u.new_ver.to_string();
    let old_c = old_w.pad_right(colored.paint(&old, |s| style(s).red().to_string()));
    let new_c = new_w.pad_left(colored.paint(&new, |s| style(s).green().to_string()));
    let arrow = colored.paint(ARROW, |s| ui::dim(s).to_string()).rendered;
    format!("  {old_c}{arrow}{new_c}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashMap;

    /// Scripted [`ShellEnv`] capturing output + recording calls, with a
    /// pre-seeded search result, name universe, classification table, and
    /// scripted review/apply outcomes, so dispatch is testable without a
    /// terminal, index, or alpm.
    #[derive(Default)]
    struct FakeEnv {
        lines: Vec<String>,
        upgrades: usize,
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
        apply_calls: usize,
    }

    impl ShellEnv for FakeEnv {
        fn print(&mut self, line: &str) {
            self.lines.push(line.to_owned());
        }
        fn upgrade(&mut self) -> Result<Vec<PkgUpgrade>> {
            self.upgrades += 1;
            Ok(self.upgrade_candidates.clone())
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
            self.apply_calls += 1;
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
                .iter()
                .any(|l| l.contains("unknown command") && l.contains("frobnicate")),
            "got: {:?}",
            env.lines
        );
    }

    #[test]
    fn help_lists_the_core_verbs() {
        let (flow, env) = dispatch_one("help");
        assert_eq!(flow, Flow::Continue);
        let joined = env.lines.join("\n");
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
        assert_eq!(env.upgrades, 1, "upgrade recomputes once");
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
        assert!(env.lines.iter().any(|l| l.contains("nothing to upgrade")));
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
        assert!(env.lines.iter().any(|l| l.contains("unknown package")));
    }

    #[test]
    fn add_dedups_silently() {
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        state.dispatch(&command::parse("add foo"), &mut env);
        assert_eq!(state.cart.items().len(), 1);
        assert!(env.lines.iter().any(|l| l.contains("already staged")));
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
        assert_eq!(env.apply_calls, 0, "apply must not run while pending");
        assert!(env.lines.iter().any(|l| l.contains("needs review")));
    }

    #[test]
    fn apply_runs_when_all_approved_and_clears_on_success() {
        let mut env = env_with(&[("glibc", Source::Repo)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add glibc"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env);
        assert_eq!(env.apply_calls, 1);
        assert!(state.cart.is_empty(), "a clean apply clears the cart");
        assert!(env.lines.iter().any(|l| l == "done"));
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
        assert!(env.lines.iter().any(|l| l.contains("didn't apply")));
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
        assert!(env.lines.iter().any(|l| l.contains("need review")));
        state.dispatch(&command::parse("approve yay-bin"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("show"), &mut env);
        assert!(env.lines.iter().any(|l| l.contains("all approved")));
    }

    #[test]
    fn syntax_error_is_reported_not_fatal() {
        let (flow, env) = dispatch_one("add \"unterminated");
        assert_eq!(flow, Flow::Continue);
        assert!(
            env.lines.iter().any(|l| l.contains("syntax error")),
            "got: {:?}",
            env.lines
        );
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
                .iter()
                .any(|l| l.starts_with("  1") && l.contains("aur/foo")),
            "row 1 should be numbered: {:?}",
            env.lines
        );
        assert!(
            env.lines
                .iter()
                .any(|l| l.contains("  2") && l.contains("extra/bar"))
        );
        assert_eq!(state.last_list.len(), 2, "the list should be remembered");
    }

    #[test]
    fn search_with_no_terms_prints_usage() {
        let (flow, env) = dispatch_one("search");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.iter().any(|l| l.contains("usage: search")));
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
        assert!(
            env.lines.iter().any(|l| l.contains("info:")),
            "got: {:?}",
            env.lines
        );
    }

    #[test]
    fn info_with_no_args_prints_usage() {
        let (flow, env) = dispatch_one("info");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.iter().any(|l| l.contains("usage: info")));
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
