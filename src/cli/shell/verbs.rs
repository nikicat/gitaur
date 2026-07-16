//! The dispatch core: map a parsed [`Command`] to side effects + control flow,
//! plus the session verbs (`search` / `info` / `upgrade` / `show` / `apply` /
//! `undo` / `redo`) and the selector-resolution glue every verb shares. The
//! cart-editing verbs live in [`super::staging`].

use super::cart::{ApplyOutcome, Cart, CartItem, StageResult};
use super::command::{Command, SystemAction};
use super::help::{HELP_TEXT, help_topic};
use super::{Flow, ListItem, ShellEnv, State, UNDO_DEPTH, View, selector};
use crate::mirror;
use crate::names::{PkgTarget, RepoName, SearchTerm};
use crate::pacman::invoke::PkgUpgrade;
use crate::system;
use crate::ui;
use std::collections::HashSet;

/// Word one `refresh` outcome — the AUR half only. The repo-database half
/// reports for itself from inside [`mirror::cmd_refresh`] (refreshed / up to
/// date / failed) and doesn't run at all when `check_repo_updates` is off,
/// so any claim about it here would double-report at best and lie at worst.
/// `None` when there is nothing to say about the AUR half — `refresh pacman`
/// scoped it out on purpose, so the repo half's own report is the whole story.
const fn refresh_message(outcome: mirror::RefreshOutcome) -> Option<&'static str> {
    match outcome {
        mirror::RefreshOutcome::Refreshed => Some("mirror + index refreshed"),
        mirror::RefreshOutcome::AurSkipped(mirror::SkipCause::NotSetUp) => {
            Some("AUR not synced — `refresh aur` runs the one-time setup")
        }
        mirror::RefreshOutcome::AurSkipped(
            mirror::SkipCause::Declined | mirror::SkipCause::NonInteractive,
        ) => Some("AUR setup skipped — run `refresh aur` when ready"),
        mirror::RefreshOutcome::AurSkipped(mirror::SkipCause::Disabled) => {
            Some("AUR refresh skipped (aur = false in config.toml)")
        }
        mirror::RefreshOutcome::AurSkipped(mirror::SkipCause::NotRequested) => None,
    }
}

/// `refresh [aur|pacman]` — re-fetch what the scope covers and reload the
/// session; the cart is left untouched (`upgrade` is the seed-the-cart
/// variant). A free function like [`system_dispatch`]: it reads no session
/// state, only the env seam. `None` is an unrecognized scope word — usage
/// line, never a silently-widened full refresh.
fn refresh_dispatch<E: ShellEnv>(scope: Option<mirror::RefreshScope>, env: &mut E) {
    match scope {
        None => env.print("usage: refresh [aur|pacman] — see `help refresh`"),
        Some(scope) => match env.refresh(scope) {
            Ok(outcome) => {
                if let Some(msg) = refresh_message(outcome) {
                    env.print(msg);
                }
            }
            Err(e) => env.print(&format!("refresh: {e}")),
        },
    }
}

/// `system <show|prune>` — the maintenance group. A free function rather than
/// a [`State`] method: it reads no session state (no cart, no lists), only the
/// env seam.
fn system_dispatch<E: ShellEnv>(action: Option<SystemAction>, env: &mut E) {
    match action {
        None => env.print("usage: system <show|prune> — see `help system`"),
        Some(SystemAction::Show) => {
            let report = env.system_usage();
            print_system_report(&report, env);
        }
        Some(SystemAction::Prune) => match env.system_prune() {
            Ok(Some(freed)) => env.print(&format!(
                "caches pruned — {freed} freed; `refresh aur` re-fetches the mirror + index"
            )),
            Ok(None) => env.print("prune cancelled — nothing removed"),
            Err(e) => env.print(&format!("prune: {e}")),
        },
    }
}

/// Render the `system show` table through `env`: one aligned row per state
/// category (size + what it holds, cache rows tagged) and a total line saying
/// what `system prune` would free. Laid out by [`ui::Grid`] — the descriptions
/// (with their `[cache]` tags) ride as unaligned tails behind the size column.
fn print_system_report<E: ShellEnv>(report: &system::Report, env: &mut E) {
    env.print(&format!("state under {}:", report.root.display()));
    let mut grid = ui::Grid::new(vec![ui::Col::left(), ui::Col::right()]).indent("  ");
    for row in &report.rows {
        let tag = if row.kind.prunable() { "  [cache]" } else { "" };
        grid.push(
            ui::GridRow::new(vec![
                ui::Cell::plain(row.kind.label()),
                ui::Cell::plain(row.size.to_string()),
            ])
            .tail(format!("  {}{tag}", row.kind.description())),
        );
    }
    grid.push(
        ui::GridRow::new(vec![
            ui::Cell::plain("total"),
            ui::Cell::plain(report.total().to_string()),
        ])
        .tail(format!(
            "  `system prune` frees the [cache] rows ({})",
            report.prunable_total(),
        )),
    );
    env.print_table(&grid.render());
}

/// Pure command dispatch: map a parsed [`Command`] to side effects + control
/// flow.
///
/// Side effects go through `env`/`self`; dispatch does no I/O of its own, so the
/// command surface and exit conditions are testable without a terminal. Each
/// argument-bearing verb is a method on [`State`] below.
// One deliberate extra inherent block: `State`'s verb handlers are split by
// concern — the dispatch core + session verbs here, the cart-editing verbs in
// `staging.rs` — and the lint can't tell a designed split from an accidental
// one.
#[allow(clippy::multiple_inherent_impl)]
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
            Command::Refresh(scope) => {
                refresh_dispatch(*scope, env);
                Flow::Continue
            }
            Command::System(action) => {
                system_dispatch(*action, env);
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
                // The env printed the numbered table itself (rendering is its
                // side of the seam); the empty case is worded here where the
                // data decision lives.
                if items.is_empty() {
                    let joined = terms
                        .iter()
                        .map(SearchTerm::as_str)
                        .collect::<Vec<_>>()
                        .join(" ");
                    env.print(&format!("no packages match `{joined}`"));
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

    /// `show`: render the staged transaction — a header, the install/removal
    /// table (delegated to the env for color + alignment + age), and whether
    /// `apply` is ready.
    ///
    /// The header and the approval summary are deterministic and stay here in
    /// the pure core (so they're unit-testable via the fake env); the table body
    /// — color, column widths, per-AUR-row age — is I/O-shaped presentation and
    /// goes through [`ShellEnv::render_cart`].
    pub(super) fn show<E: ShellEnv>(&self, env: &mut E) {
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
    pub(super) fn resolve_against_cart(&self, args: &[String]) -> Result<Vec<PkgTarget>, String> {
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
    pub(super) fn resolve_against_list<E: ShellEnv>(
        &self,
        args: &[String],
        env: &E,
    ) -> Result<Vec<PkgTarget>, String> {
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
    pub(super) fn push_undo(&mut self, before: Cart) {
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
) -> Result<Vec<PkgUpgrade>, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::assert_regex;
    use crate::cli::shell::cart::Source;
    use crate::cli::shell::command;
    use crate::cli::shell::testenv::{
        FakeEnv, cart_specs, dispatch_one, env_with, li, li_repo, up,
    };
    use crate::units::ByteSize;

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
            env.refresh_scopes,
            vec![mirror::RefreshScope::Everything],
            "a bare refresh covers everything"
        );
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

    /// `refresh aur` / `refresh pacman` narrow the scope; the words come from
    /// the one table the parser and completer share.
    #[test]
    fn refresh_scope_words_reach_the_env() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("refresh aur"), &mut env);
        state.dispatch(&command::parse("refresh pacman"), &mut env);
        assert_eq!(
            env.refresh_scopes,
            vec![mirror::RefreshScope::Aur, mirror::RefreshScope::Pacman]
        );
    }

    /// A typo'd scope prints usage and never reaches the env — it must not
    /// silently widen into a full refresh.
    #[test]
    fn refresh_with_unknown_scope_prints_usage_and_does_nothing() {
        let mut env = FakeEnv::default();
        State::default().dispatch(&command::parse("refresh pacmna"), &mut env);
        assert_eq!(env.refreshes.count(), 0);
        assert!(
            env.lines
                .any(|l| l.starts_with("usage: refresh [aur|pacman]")),
            "{:?}",
            env.lines
        );
    }

    /// `refresh pacman` scoped the AUR half out on purpose: the repo half
    /// reports for itself inside `cmd_refresh`, so the dispatch core adds no
    /// line of its own (an "AUR skipped" note would be noise).
    #[test]
    fn refresh_pacman_scope_says_nothing_about_the_aur() {
        let mut env = FakeEnv {
            refresh_outcome: Some(mirror::RefreshOutcome::AurSkipped(
                mirror::SkipCause::NotRequested,
            )),
            ..FakeEnv::default()
        };
        State::default().dispatch(&command::parse("refresh pacman"), &mut env);
        assert_eq!(env.refreshes.count(), 1);
        assert!(env.lines.is_empty(), "{:?}", env.lines);
    }

    /// A bare `refresh` in a never-synced session stays pacman-only and
    /// points at `refresh aur` — it must never read as a full refresh.
    #[test]
    fn refresh_not_set_up_words_the_skip_with_the_aur_hint() {
        let mut env = FakeEnv {
            refresh_outcome: Some(mirror::RefreshOutcome::AurSkipped(
                mirror::SkipCause::NotSetUp,
            )),
            ..FakeEnv::default()
        };
        State::default().dispatch(&command::parse("refresh"), &mut env);
        assert!(
            env.lines
                .any(|l| l.contains("AUR not synced") && l.contains("`refresh aur`")),
            "{:?}",
            env.lines
        );
        assert!(!env.lines.contains("mirror + index refreshed"));
    }

    /// A declined bootstrap is worded as a skip (with the retry hint), not as
    /// a full "mirror + index refreshed".
    #[test]
    fn refresh_decline_words_the_skip() {
        let mut env = FakeEnv {
            refresh_outcome: Some(mirror::RefreshOutcome::AurSkipped(
                mirror::SkipCause::Declined,
            )),
            ..FakeEnv::default()
        };
        State::default().dispatch(&command::parse("refresh"), &mut env);
        assert!(env.lines.contains("AUR setup skipped"));
        assert!(!env.lines.contains("mirror + index refreshed"));
    }

    /// Pacman-only mode: `refresh` words the AUR skip and claims nothing
    /// about the repo half — that half reports for itself from inside
    /// `cmd_refresh`, and doesn't run at all with `check_repo_updates` off.
    #[test]
    fn refresh_disabled_words_the_skip_without_repo_claims() {
        let mut env = FakeEnv {
            refresh_outcome: Some(mirror::RefreshOutcome::AurSkipped(
                mirror::SkipCause::Disabled,
            )),
            ..FakeEnv::default()
        };
        State::default().dispatch(&command::parse("refresh"), &mut env);
        assert!(
            env.lines
                .contains("AUR refresh skipped (aur = false in config.toml)")
        );
        assert!(!env.lines.contains("refreshed"));
    }

    #[test]
    fn system_without_action_prints_usage_and_never_prunes() {
        // The safety the two-word group exists for: neither a bare `system`
        // nor a typo'd action may fall through to the prune.
        for line in ["system", "system wat"] {
            let mut env = FakeEnv::default();
            State::default().dispatch(&command::parse(line), &mut env);
            assert!(
                env.lines.contains("usage: system"),
                "`{line}`: {:?}",
                env.lines
            );
            assert_eq!(env.prune_calls.count(), 0, "`{line}` must not prune");
        }
    }

    #[test]
    fn system_show_renders_rows_with_cache_tags_and_the_totals() {
        let mut env = FakeEnv {
            usage_rows: vec![
                system::Usage {
                    kind: system::StateKind::Mirror,
                    size: ByteSize::new(2 * 1024 * 1024 * 1024),
                },
                system::Usage {
                    kind: system::StateKind::Metrics,
                    size: ByteSize::new(1024),
                },
            ],
            ..FakeEnv::default()
        };
        State::default().dispatch(&command::parse("system show"), &mut env);
        assert!(env.lines.contains("state under /state"), "{:?}", env.lines);
        // One anchored regex per rendered row: label, aligned size, description,
        // and the [cache] tag only on the prunable row.
        assert_regex!(
            env.lines.joined(),
            r"(?m)^  mirror\s+2\.00 GiB\s+AUR git mirror\s+\[cache\]$"
        );
        assert_regex!(
            env.lines.joined(),
            r"(?m)^  metrics\s+1\.00 KiB\s+build-time history$"
        );
        // The total sums both rows; the prunable half quotes only the mirror.
        assert_regex!(
            env.lines.joined(),
            r"(?m)^  total\s+2\.00 GiB\s+`system prune` frees the \[cache\] rows \(2\.00 GiB\)$"
        );
        assert_eq!(env.prune_calls.count(), 0, "show must not prune");
    }

    #[test]
    fn system_prune_reports_the_freed_bytes() {
        let mut env = FakeEnv {
            prune_outcome: Some(ByteSize::new(3 * 1024 * 1024)),
            ..FakeEnv::default()
        };
        State::default().dispatch(&command::parse("system prune"), &mut env);
        assert_eq!(env.prune_calls.count(), 1);
        assert!(
            env.lines
                .any(|l| l.contains("3.00 MiB freed") && l.contains("`refresh aur`")),
            "{:?}",
            env.lines
        );
    }

    #[test]
    fn system_prune_declined_reports_cancellation() {
        // `prune_outcome: None` scripts the user answering N at the confirm.
        let mut env = FakeEnv::default();
        State::default().dispatch(&command::parse("system prune"), &mut env);
        assert_eq!(env.prune_calls.count(), 1, "the env owns the prompt");
        assert!(env.lines.contains("cancelled"), "{:?}", env.lines);
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
    fn search_remembers_list_and_sets_view() {
        // The printed table (numbering, alignment, worst-first order) is
        // RealEnv's side of the seam, pinned by `ui::search_table`'s tests and
        // the search PTY e2e; the pure core's job is the session state.
        let mut env = FakeEnv {
            search_result: vec![li("foo"), li("bar")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        let flow = state.dispatch(&command::parse("search foo"), &mut env);
        assert_eq!(flow, Flow::Continue);
        assert_eq!(state.search_list.len(), 2, "the list should be remembered");
        assert_eq!(state.view, View::Search, "numbers now key the search list");
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
            search_list: vec![li("foo"), li("bar")],
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
            search_list: vec![li("only")],
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

    // --- unified numbering: a bare number follows the last-shown list ---

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
