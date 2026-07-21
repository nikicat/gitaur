//! The dispatch core: map a parsed [`Command`] to side effects + control flow,
//! plus the session verbs (`search` / `info` / `upgrade` / `show` / `apply` /
//! `undo` / `redo`) and the selector-resolution glue every verb shares. The
//! cart-editing verbs live in [`super::staging`].

use super::cart::{ApplyOutcome, Approval, Cart, CartItem, StageResult};
use super::command::{Command, ConfigAction, SystemAction, unknown_note};
use super::help::{HELP_TEXT, help_topic};
use super::staging::prior_approval;
use super::{
    CartEdit, Flow, ListItem, ListSource, NumberedList, ShellEnv, State, UNDO_DEPTH, selector,
};
use crate::mirror;
use crate::names::{PkgTarget, RepoName, SearchTerm};
use crate::pacman::invoke::PkgUpgrade;
use crate::system;
use crate::ui;
use std::collections::HashSet;
use std::rc::Rc;

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

/// `config <show|set|reset>` — inspect or change a persistent config knob.
/// A free function like [`system_dispatch`]: it reads no session state, only
/// the env seam (which owns the config handle + its schema-validating edits).
/// `None` is a missing/unknown sub-verb — usage line, never a silent no-op.
/// Path/value validation lives behind the env; here we only route + prefix the
/// error uniformly.
fn config_dispatch<E: ShellEnv>(action: Option<&ConfigAction>, env: &mut E) {
    match action {
        None => env.print(
            "usage: config <show [path] | set <path> <value> | reset <path>> — see `help config`",
        ),
        // Bare `config` — teach the command rather than dump every value.
        Some(ConfigAction::Help) => env.print(&help_topic("config")),
        // The table (current/default columns, colored changes) is presentation,
        // so the env renders and prints it; the pure core only routes here and
        // prefixes an unknown-path error uniformly.
        Some(ConfigAction::Show(path)) => {
            if let Err(e) = env.config_show(path.as_ref()) {
                env.print(&format!("config: {e}"));
            }
        }
        Some(ConfigAction::Set { path, value }) => match env.config_set(path, value) {
            Ok(summary) => env.print(&summary),
            Err(e) => env.print(&format!("config: {e}")),
        },
        Some(ConfigAction::Reset(path)) => match env.config_reset(path) {
            Ok(summary) => env.print(&summary),
            Err(e) => env.print(&format!("config: {e}")),
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
        // Two tail segments — the description, then the `[cache]` flag on
        // prunable rows — gutters supplied by the grid.
        let cache = if row.kind.prunable() {
            ui::Cell::plain("[cache]")
        } else {
            ui::Cell::plain("")
        };
        grid.push(
            ui::GridRow::new(vec![
                ui::Cell::plain(row.kind.label()),
                ui::Cell::plain(row.size.to_string()),
            ])
            .tail(vec![ui::Cell::plain(row.kind.description()), cache]),
        );
    }
    grid.push(
        ui::GridRow::new(vec![
            ui::Cell::plain("total"),
            ui::Cell::plain(report.total().to_string()),
        ])
        .tail(vec![ui::Cell::plain(format!(
            "`system prune` frees the [cache] rows ({})",
            report.prunable_total(),
        ))]),
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
    pub(crate) fn dispatch<E: ShellEnv>(&mut self, cmd: &Command, env: &mut E) -> Flow {
        match cmd {
            Command::Empty => Flow::Continue,
            Command::Quit => Flow::Exit(0),
            Command::Syntax(msg) => {
                env.print(&format!("syntax error: {msg}"));
                Flow::Continue
            }
            Command::Unknown(verb) => {
                env.print(&unknown_note(verb));
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
                    self.edit_cart(|s| {
                        s.cart.clear();
                        CartEdit::Changed
                    });
                    env.print("cart cleared — `undo` to restore");
                }
                Flow::Continue
            }
            Command::Refresh(scope) => {
                self.refresh(*scope, env);
                Flow::Continue
            }
            Command::System(action) => {
                system_dispatch(*action, env);
                Flow::Continue
            }
            Command::Config(action) => {
                config_dispatch(action.as_ref(), env);
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
                // The just-printed rows become what numbers address. A
                // fruitless search printed no numbered rows, so it leaves the
                // referent alone — the table still on screen above stays
                // addressable (WYSIWYG addressing).
                if !items.is_empty() {
                    self.referent = Some(NumberedList {
                        source: ListSource::Search,
                        rows: items,
                    });
                }
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
        let targets: Vec<PkgTarget> = match self.resolve_against_list(args, env) {
            Ok(t) => t.into_iter().map(|r| r.target).collect(),
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
        // `upgrade` *replaces* the cart: `env.upgrade()` just reloaded (a fetch
        // may have moved the DBs), and the seeded set is the whole intended
        // transaction. Clear, seed, then re-freeze the resolution in one go —
        // so a mid-set resolver error rejects the whole seed and leaves the old
        // cart intact.
        let result = self.edit_and_resolve(env, |s, env| {
            s.cart.clear();
            for u in to_seed {
                let mut item = CartItem::from_upgrade(u, policy);
                // A prior session's approval covering this exact PKGBUILD
                // commit seeds the row pre-approved — see `prior_approval`.
                let prior = prior_approval(env, item.source, item.spec());
                if prior.is_some() {
                    item.approval = Approval::Approved;
                }
                if s.cart.add(item) == StageResult::Staged {
                    if let Some(pb) = prior {
                        s.cart.mark_reviewed(pb);
                    }
                    staged += 1;
                }
            }
            CartEdit::from_changed(staged > 0)
        });
        match result {
            Ok(CartEdit::Changed) => {
                let noun = if staged == 1 { "upgrade" } else { "upgrades" };
                env.print(&format!("{staged} {noun} staged"));
                // `show` prints the seeded transaction and re-arms the referent.
                self.show(env);
            }
            Ok(CartEdit::Unchanged) => env.print("nothing to upgrade"),
            Err(e) => env.print(&format!("upgrade: {e}")),
        }
    }

    /// `refresh [aur|pacman]` — re-fetch what the scope covers and reload the
    /// session. Unlike `upgrade`, it seeds nothing; but a refresh *moves the
    /// package data the cart's frozen resolution was resolved against*, so on a
    /// successful reload it **drops the cart** — the installs, removals, and
    /// resolution, the undo/redo stacks, and a transaction referent — noting the
    /// discard. `None` is an unrecognized scope word — a usage line, never a
    /// silently-widened full refresh.
    fn refresh<E: ShellEnv>(&mut self, scope: Option<mirror::RefreshScope>, env: &mut E) {
        let Some(scope) = scope else {
            env.print("usage: refresh [aur|pacman] — see `help refresh`");
            return;
        };
        match env.refresh(scope) {
            Ok(outcome) => {
                self.drop_cart_on_reload(env);
                if let Some(msg) = refresh_message(outcome) {
                    env.print(msg);
                }
            }
            Err(e) => env.print(&format!("refresh: {e}")),
        }
    }

    /// Drop the cart because a refresh moved the DBs it was resolved against:
    /// clear installs + removals + the frozen resolution + undo/redo, and drop
    /// the referent when it pointed at the (now-gone) transaction rows. Notes
    /// the discard only when the cart wasn't already empty, so a refresh on a
    /// clean session stays quiet.
    fn drop_cart_on_reload<E: ShellEnv>(&mut self, env: &mut E) {
        let had_staged = !self.cart.is_empty();
        self.cart.clear();
        self.clear_undo_history();
        if self
            .referent
            .as_ref()
            .is_some_and(|l| l.source == ListSource::Transaction)
        {
            self.referent = None;
        }
        if had_staged {
            env.print("cart cleared — refresh moved the package data it was resolved against");
        }
    }

    /// `show`: render the staged transaction — a header, the install/removal
    /// table (delegated to the env for color + alignment + age), and whether
    /// `apply` is ready — and make the just-rendered rows what bare numbers
    /// address.
    ///
    /// The header and the approval summary are deterministic and stay here in
    /// the pure core (so they're unit-testable via the fake env); the table body
    /// — color, column widths, per-AUR-row age — is I/O-shaped presentation and
    /// goes through [`ShellEnv::render_cart`]. This is the one place the
    /// transaction prints *numbered*, so it is the one place the referent flips
    /// to it — render and capture can't drift.
    pub(super) fn show<E: ShellEnv>(&mut self, env: &mut E) {
        if self.cart.is_empty() {
            env.print("cart is empty — `add <pkg>` to stage an install");
            return;
        }
        env.print(&self.txn_header());
        env.render_cart(&self.cart);
        env.print(&self.approval_status());
        self.referent = Some(NumberedList {
            source: ListSource::Transaction,
            rows: self.cart_as_list(),
        });
    }

    /// One-line cart status — the same header counts and approval standing
    /// `show` prints, minus the table. Printed by the quiet cart-editing verbs
    /// (`add`/`drop`/`keep`/`remove`/`approve`/`review`) after a real change,
    /// so the cart's standing is always on screen without a table dump — and
    /// without printing row numbers that aren't addressable.
    pub(super) fn summarize<E: ShellEnv>(&self, env: &mut E) {
        if self.cart.is_empty() {
            env.print("cart is empty");
            return;
        }
        env.print(&self.txn_header());
        env.print(&self.approval_status());
    }

    /// The prompt for the next read line — ambient cart state (the classic
    /// fix for hidden state: carry it in the prompt, don't reprint it), so
    /// the user never needs a `show` just to remember where the transaction
    /// stands. Empty cart keeps the plain `aurox> `; a staged cart counts its
    /// rows (installs + removals) and any review gates still open.
    pub(super) fn prompt(&self) -> String {
        if self.cart.is_empty() {
            return "aurox> ".to_owned();
        }
        let staged = self.cart.items().len() + self.cart.removals().len();
        let pending = self.cart.pending_review().len();
        if pending == 0 {
            format!("aurox [{staged} staged]> ")
        } else {
            format!("aurox [{staged} staged, {pending} to review]> ")
        }
    }

    /// The transaction header line, shared by `show` and [`Self::summarize`]
    /// so the two can't drift apart in wording.
    fn txn_header(&self) -> String {
        format!(
            "transaction — {} to install, {} to remove",
            self.cart.items().len(),
            self.cart.removals().len()
        )
    }

    /// The approval standing + next step, shared by `show` and
    /// [`Self::summarize`]. A single pending package is named with its exact
    /// command filled in — the shell knows the name, so the user shouldn't
    /// have to substitute a `<sel>` placeholder; the plural points at bare
    /// `review`, which walks every pending item.
    fn approval_status(&self) -> String {
        let pending = self.cart.pending_review();
        match *pending.as_slice() {
            [] => "all approved — run `apply`".to_owned(),
            [one] => {
                let name = one.spec();
                format!("{name} needs review — run `review {name}` (or `approve {name}`)")
            }
            ref many => format!(
                "{} packages need review — run `review` to walk them (or `approve <sel>`)",
                many.len()
            ),
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
            let names: Vec<&str> = pending.iter().map(|i| i.spec().as_str()).collect();
            env.print(&format!(
                "needs review: {} — run `review <sel>` or `approve <sel>`",
                names.join(", ")
            ));
            return;
        }
        let run = match env.apply(&self.cart) {
            Ok(run) => run,
            Err(e) => {
                env.print(&format!("apply: {e}"));
                return;
            }
        };
        // Fold the run's review knowledge back FIRST, whatever the outcome: a
        // pulled-in dep's diff approved during a failed run must not re-prompt
        // on the retry (and stays known for later re-adds after a success).
        self.cart.absorb_reviewed(run.reviewed);
        match run.outcome {
            ApplyOutcome::Declined => env.print("apply cancelled — cart kept"),
            ApplyOutcome::Succeeded => {
                self.cart.clear_applied();
                // The transaction ran: the cart is a new epoch, so pre-apply
                // undo snapshots (which would re-stage now-installed packages)
                // no longer make sense.
                self.clear_undo_history();
                env.print("done");
            }
            ApplyOutcome::Failed { installed } => {
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
                self.show(env);
            }
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
    /// Numbers, though, name rows of the last numbered table printed (see
    /// [`NumberedList`]), so a bare `3` means the same row it would for any
    /// other verb — the one the user can see.
    pub(super) fn resolve_against_cart(
        &self,
        args: &[String],
    ) -> Result<Vec<selector::Resolved>, String> {
        let rows: Vec<RepoRow> = self.cart.items().iter().map(RepoRow::from).collect();
        let args = expand_repo_tokens(args, &rows);
        let universe: Vec<PkgTarget> = rows.iter().map(|r| r.target.clone()).collect();
        selector::resolve(&args, self.referent.as_ref(), &universe)
    }

    /// Resolve selector `args` for a list verb (`add`, `info`, `remove`): a repo
    /// name selects every row from that repo in the last numbered table,
    /// numbers/ranges index that table, and names/globs resolve against the full
    /// name universe (so you can `add` anything installable, not just what's
    /// shown).
    pub(super) fn resolve_against_list<E: ShellEnv>(
        &self,
        args: &[String],
        env: &E,
    ) -> Result<Vec<selector::Resolved>, String> {
        let rows: Vec<RepoRow> = self.referent_rows().iter().map(RepoRow::from).collect();
        let args = expand_repo_tokens(args, &rows);
        selector::resolve(&args, self.referent.as_ref(), env.names())
    }

    /// The staged cart as selector rows — the same rows, in the same order,
    /// that `show` renders. Snapshotted into the referent at `show` time, so
    /// the numbers the user reads stay bound to these packages until the next
    /// numbered table prints.
    fn cart_as_list(&self) -> Vec<ListItem> {
        self.cart.items().iter().map(ListItem::from).collect()
    }

    /// The referent's rows, or an empty slice before any numbered table was
    /// printed (repo-token expansion iterates these; number resolution itself
    /// goes through the referent for kind-aware errors).
    fn referent_rows(&self) -> &[ListItem] {
        self.referent.as_ref().map_or(&[], |l| &l.rows)
    }

    /// Run one undoable cart edit: snapshot the cart, run `edit`, and push the
    /// snapshot onto the undo stack iff the edit reports it changed something
    /// (so a no-op never consumes an undo step). Returns that report for the
    /// caller's follow-up printing. This is the *single* clone site for the
    /// undo feature — a cart-editing verb structurally cannot forget the
    /// snapshot or push an unchanged one.
    ///
    /// For an install-set change use [`Self::edit_and_resolve`] instead, which
    /// also re-freezes the resolution (or rejects). `edit_cart` is for edits
    /// that don't need a resolve: `clear` (empties the cart, no resolution) and
    /// approval-only moves are handled directly.
    pub(super) fn edit_cart(&mut self, edit: impl FnOnce(&mut Self) -> CartEdit) -> CartEdit {
        let before = self.cart.clone();
        let outcome = edit(self);
        if outcome == CartEdit::Changed {
            self.push_undo(before);
        }
        outcome
    }

    /// Run one undoable install-set change and **re-freeze the whole-cart
    /// resolution**: snapshot, run `edit`, then (if it changed the cart)
    /// re-resolve via [`ShellEnv::stage_plan`]. On success the fresh resolution
    /// is stored and the snapshot pushed for `undo`; on a resolver / conflict
    /// error the cart is **rolled back** to the snapshot and the `Err` bubbles,
    /// so the caller rejects the command and an incoherent cart is never kept.
    ///
    /// A change that empties the cart (dropping the last row) skips the resolve
    /// — there's nothing to apply, and the stale resolution is never read on an
    /// empty cart. The single write site for [`Cart::set_resolution`].
    pub(super) fn edit_and_resolve<E: ShellEnv>(
        &mut self,
        env: &mut E,
        edit: impl FnOnce(&mut Self, &mut E) -> CartEdit,
    ) -> Result<CartEdit, String> {
        let before = self.cart.clone();
        if edit(self, env) == CartEdit::Unchanged {
            return Ok(CartEdit::Unchanged);
        }
        if self.cart.is_empty() {
            self.push_undo(before);
            return Ok(CartEdit::Changed);
        }
        // The resolver error is surfaced as a message (matching the selector
        // verbs' `Result<_, String>`); the reject rolls the cart back.
        match env.stage_plan(&self.cart) {
            Ok(resolved) => {
                self.cart.set_resolution(Rc::new(resolved));
                self.push_undo(before);
                Ok(CartEdit::Changed)
            }
            Err(e) => {
                self.cart = before;
                Err(e.to_string())
            }
        }
    }

    /// Snapshot the pre-change cart onto the `undo` stack (bounded) and discard
    /// any redo branch. [`Self::edit_cart`]'s push half — only ever called with
    /// the cart as it was before an edit that really changed something.
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
                env.print("undone — `redo` to reapply");
                // Present the restored cart (and re-arm the referent): the
                // user didn't enumerate this change, so showing it is the job.
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

impl RepoRow {
    /// Whether this row belongs to the repo named by `word`
    /// (case-insensitive) — the predicate a repo-name selector filters on.
    fn matches_repo(&self, word: &str) -> bool {
        self.repo
            .as_ref()
            .is_some_and(|repo| repo.as_str().eq_ignore_ascii_case(word))
    }
}

// The three row sources a repo filter runs over. Each conversion owns its
// clones once, at a named seam, instead of per-field `.clone()`s at the call
// sites (the shell-layer clone policy: clone freely, but at one place).
impl From<&CartItem> for RepoRow {
    fn from(it: &CartItem) -> Self {
        Self {
            target: it.spec().clone(),
            repo: Some(it.repo_label()),
        }
    }
}

impl From<&ListItem> for RepoRow {
    fn from(it: &ListItem) -> Self {
        Self {
            target: it.target.clone(),
            repo: it.repo.clone(),
        }
    }
}

impl From<&PkgUpgrade> for RepoRow {
    fn from(u: &PkgUpgrade) -> Self {
        Self {
            target: PkgTarget::from(&u.name),
            repo: Some(u.repo.clone()),
        }
    }
}

/// An upgrade-candidate row as a selector list row (`select_from_candidates`
/// numbers the candidates it filters).
impl From<&RepoRow> for ListItem {
    fn from(r: &RepoRow) -> Self {
        Self {
            target: r.target.clone(),
            repo: r.repo.clone(),
        }
    }
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
                .filter(|r| r.matches_repo(a))
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
    let rows: Vec<RepoRow> = candidates.iter().map(RepoRow::from).collect();
    let args = expand_repo_tokens(args, &rows);
    let list = NumberedList {
        source: ListSource::UpgradeCandidates,
        rows: rows.iter().map(ListItem::from).collect(),
    };
    let universe: Vec<PkgTarget> = rows.iter().map(|r| r.target.clone()).collect();
    let picked = selector::resolve(&args, Some(&list), &universe)?;
    // Join back to the candidates in target space: each candidate's name
    // lifts through the declared `PkgName → PkgTarget` conversion, so the
    // match never drops to raw strings.
    let picked: HashSet<PkgTarget> = picked.into_iter().map(|r| r.target).collect();
    Ok(candidates
        .iter()
        .filter(|u| picked.contains(&PkgTarget::from(&u.name)))
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
        FakeEnv, cart_specs, dispatch_one, env_with, li, li_repo, state_showing, up,
    };
    use crate::names::PkgBase;
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

    /// An AUR upgrade row whose exact PKGBUILD commit a prior session already
    /// approved seeds pre-approved (the persistent reviewed set restored),
    /// with its pkgbase in the session set so `apply` won't re-prompt; rows
    /// the store doesn't cover still gate.
    #[test]
    fn upgrade_seeds_previously_approved_aur_rows_pre_approved() {
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("aur", "yay-bin"), up("aur", "paru-bin")],
            ..FakeEnv::default()
        };
        env.prior_approvals.insert("yay-bin".into());
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env);
        assert_eq!(state.cart.pending_review().len(), 1);
        assert_eq!(state.cart.pending_review()[0].spec(), "paru-bin");
        assert!(state.cart.reviewed().contains(&PkgBase::from("yay-bin")));
    }

    #[test]
    fn upgrade_with_selector_seeds_only_the_subset() {
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("core", "glibc"), up("aur", "yay-bin")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade yay-bin"), &mut env);
        let specs: Vec<&str> = state
            .cart
            .items()
            .iter()
            .map(|i| i.spec().as_str())
            .collect();
        assert_eq!(specs, vec!["yay-bin"]);
    }

    #[test]
    fn upgrade_with_nothing_to_do_stages_nothing() {
        let (flow, env) = dispatch_one("upgrade");
        assert_eq!(flow, Flow::Continue);
        assert!(env.lines.contains("nothing to upgrade"));
    }

    #[test]
    fn refresh_drops_the_cart_it_can_no_longer_trust() {
        // The canary for the resolve-at-add invariant: a refresh moves the DBs
        // the cart's frozen resolution was resolved against, so it can't be
        // applied any more. `refresh` drops the whole cart (and says so) rather
        // than leave a stale transaction — the inverse of the old "refresh
        // leaves the cart intact" contract.
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
        assert!(state.cart.is_empty(), "refresh drops the stale cart");
        assert!(
            env.lines.contains("cart cleared"),
            "the discard is announced: {:?}",
            env.lines
        );
        assert!(env.lines.contains("refreshed"));
    }

    #[test]
    fn refresh_on_an_empty_cart_stays_quiet_about_the_cart() {
        // Nothing staged → no cart to drop → no "cart cleared" note (just the
        // refresh outcome).
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("refresh"), &mut env);
        assert!(
            !env.lines.contains("cart cleared"),
            "an empty cart says nothing about being cleared: {:?}",
            env.lines
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

    /// A knob row in the plain-paint config table: a line whose columns are the
    /// path, the current value, and the default value (the fake pins `Plain`).
    fn config_row(env: &FakeEnv, path: &str, current: &str, default: &str) -> bool {
        env.lines.any(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            cols == [path, current, default]
        })
    }

    #[test]
    fn config_show_renders_the_current_and_default_columns() {
        let (flow, env) = dispatch_one("config show");
        assert_eq!(flow, Flow::Continue);
        // A header, then one row per knob with current == default (unset).
        assert!(
            env.lines
                .any(|l| l.split_whitespace().collect::<Vec<_>>()
                    == ["setting", "current", "default"]),
            "{:?}",
            env.lines
        );
        assert!(config_row(&env, "aur", "true", "true"), "{:?}", env.lines);
        assert!(config_row(&env, "color", "auto", "auto"), "{:?}", env.lines);
    }

    #[test]
    fn config_set_persists_and_a_later_show_reflects_it() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("config set color never"), &mut env);
        assert!(
            env.lines
                .any(|l| l.contains("color = never") && l.contains("was auto")),
            "{:?}",
            env.lines
        );
        env.lines.clear();
        state.dispatch(&command::parse("config show color"), &mut env);
        // The current column now shows the override against the `auto` default.
        assert!(
            config_row(&env, "color", "never", "auto"),
            "{:?}",
            env.lines
        );
    }

    #[test]
    fn config_set_rejects_a_bad_value_without_persisting() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("config set color bogus"), &mut env);
        assert!(
            env.lines.any(|l| l.starts_with("config:")),
            "a rejected set reports an error: {:?}",
            env.lines
        );
        env.lines.clear();
        state.dispatch(&command::parse("config show color"), &mut env);
        assert!(
            config_row(&env, "color", "auto", "auto"),
            "the file stayed unchanged (current == default): {:?}",
            env.lines
        );
    }

    #[test]
    fn config_reset_clears_an_override() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("config set index_threads 8"), &mut env);
        state.dispatch(&command::parse("config reset index_threads"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("config show index_threads"), &mut env);
        assert!(
            config_row(&env, "index_threads", "4", "4"),
            "reset returns the knob to its default (current == default): {:?}",
            env.lines
        );
    }

    #[test]
    fn bare_config_prints_the_help_topic_not_a_value_dump() {
        let (flow, env) = dispatch_one("config");
        assert_eq!(flow, Flow::Continue);
        let joined = env.lines.joined();
        assert!(
            joined.contains("config <show|set|reset>"),
            "bare config shows help: {joined}"
        );
        // Not the value table.
        assert!(
            !env.lines
                .any(|l| l.split_whitespace().collect::<Vec<_>>()
                    == ["setting", "current", "default"]),
            "bare config must not dump the table: {:?}",
            env.lines
        );
    }

    #[test]
    fn config_without_a_valid_subcommand_prints_usage() {
        // A missing value / unknown sub-verb never silently no-ops.
        for line in ["config wat", "config set color", "config reset"] {
            let mut env = FakeEnv::default();
            State::default().dispatch(&command::parse(line), &mut env);
            assert!(
                env.lines.any(|l| l.starts_with("usage: config")),
                "`{line}`: {:?}",
                env.lines
            );
        }
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
    fn apply_folds_mid_run_reviews_back_whatever_the_outcome() {
        // The reviewed-set loss: a pulled-in dep's PKGBUILD approved *during*
        // a failed apply must not re-prompt on the retry — the run's review
        // knowledge is folded into the cart on every outcome.
        let mut env = env_with(&[("a", Source::Aur)]);
        env.apply_outcome = Some(ApplyOutcome::Failed {
            installed: Vec::new(),
        });
        env.apply_reviewed = std::iter::once(PkgBase::from("some-dep")).collect();
        let mut state = State::default();
        state.dispatch(&command::parse("add a"), &mut env);
        state.dispatch(&command::parse("approve a"), &mut env);
        state.dispatch(&command::parse("apply"), &mut env);
        assert!(
            state.cart.reviewed().contains(&PkgBase::from("some-dep")),
            "the mid-run approval must survive the failed run"
        );
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
        assert!(env.lines.contains("yay-bin needs review"));
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
    fn search_snapshots_the_printed_rows_as_the_referent() {
        // The printed table (numbering, alignment, worst-first order) is
        // RealEnv's side of the seam, pinned by `ui::search_table`'s tests and
        // the search PTY e2e; the pure core's job is the referent snapshot.
        let mut env = FakeEnv {
            search_result: vec![li("foo"), li("bar")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        let flow = state.dispatch(&command::parse("search foo"), &mut env);
        assert_eq!(flow, Flow::Continue);
        let referent = state.referent.as_ref().expect("search sets the referent");
        assert_eq!(referent.rows.len(), 2, "the printed rows are the snapshot");
        assert_eq!(referent.source, ListSource::Search);
    }

    #[test]
    fn fruitless_search_keeps_the_previous_referent() {
        // A no-hit search prints no numbered rows, so the table still on
        // screen above stays addressable — `add 1` picks from it.
        let mut env = env_with(&[("foo", Source::Aur)]);
        env.search_result = vec![li_repo("aur", "foo")];
        let mut state = State::default();
        state.dispatch(&command::parse("search foo"), &mut env);
        env.search_result = Vec::new();
        state.dispatch(&command::parse("search zzz"), &mut env);
        assert!(env.lines.contains("no packages match"));
        state.dispatch(&command::parse("add 1"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["foo"],
            "row 1 still names the visible earlier result"
        );
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
        let mut state = state_showing(vec![li("foo"), li("bar")]);
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
        let mut state = state_showing(vec![li("only")]);
        state.dispatch(&command::parse("info 9"), &mut env);
        assert!(env.info_calls.is_empty(), "must not show on a bad index");
        assert!(
            env.lines
                .contains("info: no row 9 — the search list has 1 row"),
            "the error names the list numbers refer to: {:?}",
            env.lines
        );
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

    // --- WYSIWYG addressing: a number names a row of the last numbered table ---

    #[test]
    fn add_by_number_keeps_pointing_at_the_search_list_across_adds() {
        // Working through a search list: `add` prints no numbered table, so
        // `add 1` then `add 3` both keep naming the search rows (the classic
        // "search, then add a few" flow).
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
            "1 and 3 keep naming the search rows"
        );
    }

    #[test]
    fn number_without_a_printed_table_errors_helpfully() {
        // Fresh session, staged by name, nothing numbered ever printed: a bare
        // number has no referent. Guessing "the (invisible, sorted) cart" is
        // the silent-wrong-target class this design removes — error and point
        // at `show` instead.
        let mut env = env_with(&[("foo", Source::Aur), ("bar", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar"), &mut env);
        state.dispatch(&command::parse("drop 1"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["bar", "foo"],
            "nothing may be dropped on a blind number"
        );
        assert!(
            env.lines.contains("no numbered list is up"),
            "got: {:?}",
            env.lines
        );
        // `show` prints the numbered cart; the number now works.
        state.dispatch(&command::parse("show"), &mut env);
        state.dispatch(&command::parse("drop 1"), &mut env);
        assert_eq!(cart_specs(&state), vec!["foo"], "`drop 1` = shown row 1");
    }

    #[test]
    fn drop_by_number_indexes_the_shown_cart() {
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("aur", "bar"), up("aur", "foo")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env); // shows cart: [bar, foo]
        state.dispatch(&command::parse("drop 1"), &mut env);
        assert_eq!(
            cart_specs(&state),
            vec!["foo"],
            "`drop 1` drops shown row 1"
        );
    }

    #[test]
    fn snapshot_pins_numbers_while_the_cart_shrinks() {
        // Working down a printed table: `drop 1` narrows the cart (quietly —
        // no re-numbering), so `drop 2` must still hit the package *printed*
        // at row 2, not "current row 2 of the live cart".
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("aur", "bar"), up("aur", "foo"), up("aur", "qux")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env); // shows [bar, foo, qux]
        state.dispatch(&command::parse("drop 1"), &mut env); // bar gone; live cart: [foo, qux]
        state.dispatch(&command::parse("drop 2"), &mut env); // printed row 2 = foo
        assert_eq!(
            cart_specs(&state),
            vec!["qux"],
            "row 2 names foo as printed, though foo is live row 1 by now"
        );
    }

    #[test]
    fn stale_snapshot_row_is_a_clean_miss() {
        // A number whose package already left the cart misses by name — it
        // must never slide onto whatever occupies that index now.
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("aur", "bar"), up("aur", "foo")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env); // shows [bar, foo]
        state.dispatch(&command::parse("drop 1"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("drop 1"), &mut env);
        assert!(
            env.lines.contains("row 1 (bar) is no longer staged"),
            "the stale row misses as bar, the package printed there: {:?}",
            env.lines
        );
        assert_eq!(cart_specs(&state), vec!["foo"], "foo must not be hit");
    }

    #[test]
    fn show_switches_numbering_from_the_search_list_to_the_cart() {
        let mut env = env_with(&[("staged", Source::Aur)]);
        env.search_result = vec![li_repo("aur", "searched")];
        let mut state = State::default();
        state.dispatch(&command::parse("add staged"), &mut env); // cart = [staged]
        state.dispatch(&command::parse("search x"), &mut env); // referent = search rows
        state.dispatch(&command::parse("info 1"), &mut env);
        assert_eq!(
            env.info_calls.last(),
            Some(&vec![PkgTarget::new("searched")]),
            "after `search`, `1` is the search row"
        );
        state.dispatch(&command::parse("show"), &mut env); // referent = shown cart
        state.dispatch(&command::parse("info 1"), &mut env);
        assert_eq!(
            env.info_calls.last(),
            Some(&vec![PkgTarget::new("staged")]),
            "after `show`, `1` is the cart row"
        );
    }

    #[test]
    fn show_on_an_empty_cart_keeps_the_referent() {
        // "cart is empty" carries no row numbers, so the search table still on
        // screen stays what numbers name.
        let mut env = env_with(&[("foo", Source::Aur)]);
        env.search_result = vec![li_repo("aur", "foo")];
        let mut state = State::default();
        state.dispatch(&command::parse("search x"), &mut env);
        state.dispatch(&command::parse("show"), &mut env);
        assert!(env.lines.contains("cart is empty"));
        state.dispatch(&command::parse("add 1"), &mut env);
        assert_eq!(cart_specs(&state), vec!["foo"]);
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

    // --- prompt: ambient cart state ---

    #[test]
    fn prompt_carries_the_cart_standing() {
        let mut env = env_with(&[("glibc", Source::Repo), ("yay-bin", Source::Aur)]);
        let mut state = State::default();
        assert_eq!(
            state.prompt(),
            "aurox> ",
            "empty cart keeps the plain prompt"
        );
        state.dispatch(&command::parse("add glibc yay-bin"), &mut env);
        assert_eq!(
            state.prompt(),
            "aurox [2 staged, 1 to review]> ",
            "counts + the open review gate"
        );
        state.dispatch(&command::parse("approve yay-bin"), &mut env);
        assert_eq!(
            state.prompt(),
            "aurox [2 staged]> ",
            "gates cleared drop the review part"
        );
        state.dispatch(&command::parse("remove oldpkg"), &mut env);
        assert_eq!(
            state.prompt(),
            "aurox [3 staged]> ",
            "staged removals count as rows"
        );
        state.dispatch(&command::parse("clear"), &mut env);
        assert_eq!(
            state.prompt(),
            "aurox> ",
            "clear returns to the plain prompt"
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
