//! The cart-staging verbs: `add` / `drop` / `keep` / `remove` / `approve` /
//! `review` — the [`State`] handlers that edit the staged transaction. The
//! dispatch match and the session verbs (`search`, `upgrade`, `apply`, …)
//! stay with the dispatch core; these handlers resolve their selectors
//! through it and mutate only the cart + undo stacks.

use super::cart::{
    Approval, ApproveResult, CartItem, KeepResult, ReviewOutcome, Source, StageClass, StageResult,
    UnstageResult,
};
use super::selector::Resolved;
use super::{ListSource, ShellEnv, State};
use crate::build::review;
use crate::index;
use crate::names::{PkgName, PkgTarget, RepoName};

/// The ack for one unstaged row: number-picked targets echo their row, so the
/// binding the user just used ("2 = 3dslicer-bin") is confirmed in passing.
fn drop_ack(r: &Resolved) -> String {
    match r.row {
        Some(n) => format!("dropped {} (row {n})", r.target.as_str()),
        None => format!("dropped {}", r.target.as_str()),
    }
}

// One deliberate extra inherent block: `State`'s verb handlers are split by
// concern — the cart-editing verbs here, the dispatch core + session verbs in
// the shell root — and the lint can't tell a designed split from an
// accidental one.
#[allow(clippy::multiple_inherent_impl)]
impl State {
    /// `add <sel…>`: classify each selected target and stage it. Selectors
    /// resolve against the last numbered table (numbers) + the full name
    /// universe (names/globs), so you can `add` anything installable. `add`
    /// prints no numbered table, so the referent is untouched and a run of
    /// `add`s keeps working through a search list.
    pub(super) fn add<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
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
        let changed = self.edit_cart(|s| {
            let mut changed = false;
            let mut any_unknown = false;
            for t in targets.into_iter().map(|r| r.target) {
                let Some(StageClass { source, repo }) = env.classify(&t) else {
                    env.print(&format!("unknown package `{}` — not staged", t.as_str()));
                    any_unknown = true;
                    continue;
                };
                let name = t.as_str().to_owned();
                // Show the concrete repo (`core`/`extra`) when known, else the
                // coarse source label.
                let label = repo
                    .clone()
                    .map_or_else(|| source.label().to_owned(), RepoName::into_inner);
                match s.cart.add(CartItem::new(t, source, repo, policy)) {
                    StageResult::Staged => {
                        env.print(&format!("staged {name} ({label})"));
                        changed = true;
                    }
                    StageResult::AlreadyStaged => {
                        env.print(&format!("{name} is already staged"));
                    }
                }
            }
            // With the AUR enabled but unsynced, "unknown" may just mean "only
            // in the AUR" — one nudge for the whole batch. Pacman-only mode is
            // a standing choice and stays quiet.
            if any_unknown && env.aur_state() == index::AurState::NotSetUp {
                env.print(
                    "unknown names may be in the AUR — `refresh aur` syncs it (one-time ~2 GiB)",
                );
            }
            changed
        });
        // One status line, not a table dump: the cart's standing stays on
        // screen without printing row numbers that aren't addressable (the
        // shell-ux plan's quiet-mutation rule). Skipped when nothing actually
        // changed (all already-staged / unknown), so a no-op `add` stays quiet.
        if changed {
            self.summarize(env);
        }
    }

    /// `drop <sel…>`: unstage installs from the cart. Names/globs match staged
    /// specs; numbers name rows of the last numbered table (see
    /// [`super::NumberedList`]) — a snapshot, so working down a shown
    /// transaction (`drop 2`, `drop 4`) hits the rows as printed even as the
    /// cart shrinks. `show` re-numbers.
    pub(super) fn discard<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
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
        let changed = self.edit_cart(|s| {
            let mut changed = false;
            for r in targets {
                match s.cart.unstage(&r.target) {
                    UnstageResult::Unstaged => {
                        env.print(&drop_ack(&r));
                        changed = true;
                    }
                    UnstageResult::NotStaged => env.print(&s.miss_note(&r)),
                }
            }
            changed
        });
        // One status line (or "cart is empty" once the last row goes) — see
        // `add` for the quiet-mutation rule.
        if changed {
            self.summarize(env);
        }
    }

    /// `keep <sel…>`: keep only the selected install rows, dropping every other
    /// staged install — the inverse of `drop`, for narrowing a large cart down to
    /// a few packages (`upgrade`, then `keep glibc firefox`). Selectors resolve
    /// against the cart, exactly like `drop`; staged removals are untouched. A
    /// selector matching nothing leaves the cart intact rather than emptying it.
    pub(super) fn keep<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
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
        let changed = self.edit_cart(|s| match s.cart.keep(targets.iter().map(|r| &r.target)) {
            KeepResult::NoMatch => {
                env.print("keep: nothing in the cart matched — cart unchanged");
                false
            }
            KeepResult::Kept { dropped } if dropped.is_empty() => {
                env.print("keep: every staged package is already kept — nothing dropped");
                false
            }
            KeepResult::Kept { dropped } => {
                for spec in &dropped {
                    env.print(&format!("dropped {}", spec.as_str()));
                }
                true
            }
        });
        if changed {
            self.summarize(env);
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
    pub(super) fn remove<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
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
        let changed = self.edit_cart(|s| {
            let mut changed = false;
            for t in targets.into_iter().map(|r| r.target) {
                // `Some(is_upgrade)` when the target is a staged install row.
                match s.cart.item(&t).map(|i| i.upgrade.is_some()) {
                    // A fresh-install row isn't installed — you can't
                    // uninstall it. Point at `drop`, which is what "get rid
                    // of this cart row" means, and stage nothing.
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
                        s.cart.unstage(&t);
                        changed = true;
                        let name = PkgName::from(t.into_inner());
                        match s.cart.stage_remove(name.clone()) {
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
                match s.cart.stage_remove(name.clone()) {
                    StageResult::Staged => {
                        env.print(&format!("staged removal of {name}"));
                        changed = true;
                    }
                    StageResult::AlreadyStaged => {
                        env.print(&format!("{name} is already staged for removal"));
                    }
                }
            }
            changed
        });
        // One status line (its counts include the new "will remove" row) — see
        // `add` for the quiet-mutation rule.
        if changed {
            self.summarize(env);
        }
    }

    /// `approve <sel…>` / `approve *`: mark staged AUR items approved without
    /// opening a diff. Repo items are already approved; selectors resolve
    /// against the cart (`*` matches every staged item).
    pub(super) fn approve<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
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
        let changed = self.edit_cart(|s| {
            let mut changed = false;
            for r in targets {
                let t = &r.target;
                match s.cart.approve(t) {
                    ApproveResult::Approved => {
                        if let Some(pb) = env.pkgbase_of(t) {
                            s.cart.mark_reviewed(pb);
                        }
                        env.print(&format!("approved {}", t.as_str()));
                        changed = true;
                    }
                    ApproveResult::AlreadyApproved => {
                        env.print(&format!("{} is already approved", t.as_str()));
                    }
                    ApproveResult::NotStaged => env.print(&s.miss_note(&r)),
                }
            }
            changed
        });
        // The status line surfaces the "all approved — run `apply`" moment the
        // instant the last gate clears (or how many gates remain).
        if changed {
            self.summarize(env);
        }
    }

    /// `review [sel…]`: open each selected AUR item's PKGBUILD (diff-against-
    /// installed or full) and approve/skip per the user's call. With no
    /// selector, walk the whole cart — every AUR item still awaiting review —
    /// so `review` alone starts the review loop. Repo items have no PKGBUILD;
    /// already-approved items are left alone; an abort stops the pass.
    pub(super) fn review<E: ShellEnv>(&mut self, args: &[String], env: &mut E) {
        let targets = if args.is_empty() {
            // Collect owned targets so the `self.cart` borrow from
            // `pending_review` is released before the loop mutates it.
            let pending: Vec<Resolved> = self
                .cart
                .pending_review()
                .iter()
                .map(|i| Resolved {
                    target: i.spec().clone(),
                    row: None,
                })
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
        let approved_any = self.edit_cart(|s| {
            let mut approved_any = false;
            // Flips to `Auto` once the user picks "approve all": the remaining
            // AUR items clear without opening another diff.
            let mut prompting = review::Prompting::default();
            for r in targets {
                let t = &r.target;
                // Copy out (source, approval) so the cart isn't borrowed across
                // the `env.review` call (which then mutates the cart on
                // approval).
                match s.cart.item(t).map(|i| (i.source, i.approval)) {
                    // A selector that resolved to something unstaged is worth a
                    // note (it was silently skipped before) — with row
                    // provenance when a number picked it.
                    None => env.print(&s.miss_note(&r)),
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
                            s.approve_reviewed(t, env);
                            approved_any = true;
                            continue;
                        }
                        match env.review(t) {
                            Ok(ReviewOutcome::Approved) => {
                                s.approve_reviewed(t, env);
                                approved_any = true;
                            }
                            Ok(ReviewOutcome::ApprovedAll) => {
                                s.approve_reviewed(t, env);
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
            approved_any
        });
        // Same status line as `approve` — a review pass that cleared the last
        // gate announces readiness without a `show`.
        if approved_any {
            self.summarize(env);
        }
    }

    /// Word a cart-verb miss for one resolved selector: name-picked targets
    /// read as before ("foo isn't staged"), row-picked ones name their row —
    /// distinguishing the stale-snapshot case (the transaction row's package
    /// has since left the cart) from numbers that never referred to the cart
    /// at all (search rows), which get the `show` pointer. This is the message
    /// that teaches what a number means when the guess was wrong.
    fn miss_note(&self, r: &Resolved) -> String {
        let t = r.target.as_str();
        match (r.row, self.referent.as_ref().map(|l| l.source)) {
            (Some(n), Some(ListSource::Transaction)) => {
                format!("row {n} ({t}) is no longer staged")
            }
            (Some(n), Some(source)) => format!(
                "row {n} of the {} ({t}) isn't staged — `show` numbers the cart",
                source.label()
            ),
            _ => format!("{t} isn't staged"),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cli::shell::testenv::{FakeEnv, cart_specs, dispatch_one, env_with, li_repo, up};
    use crate::cli::shell::{Flow, command};
    use crate::names::PkgBase;

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
        // With a ready index "unknown" is authoritative — no AUR speculation.
        assert!(!env.lines.contains("may be in the AUR"));
    }

    /// With the AUR enabled but unsynced, an unknown `add` target may simply
    /// live there: one nudge per batch points at `refresh` (never a prompt —
    /// staging must stay cheap). Pacman-only mode keeps the standing silence.
    #[test]
    fn add_unknown_nudges_at_the_aur_only_when_not_set_up() {
        let mut env = FakeEnv {
            aur_state: Some(index::AurState::NotSetUp),
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("add nope nada"), &mut env);
        assert!(env.lines.contains("may be in the AUR"));
        assert_eq!(
            env.lines.count_containing("may be in the AUR"),
            1,
            "one nudge for the whole batch"
        );

        let mut env = FakeEnv {
            aur_state: Some(index::AurState::Disabled),
            ..FakeEnv::default()
        };
        state.dispatch(&command::parse("add nope"), &mut env);
        assert!(!env.lines.contains("may be in the AUR"));
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
        let specs: Vec<&str> = state
            .cart
            .items()
            .iter()
            .map(|i| i.spec().as_str())
            .collect();
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
        // One status line for the narrowed cart, like `drop` — no table.
        assert!(env.lines.contains("transaction — 1 to install"));
        assert_eq!(env.render_calls.count(), 0, "keep must not draw the table");
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
    fn remove_stages_an_uninstall() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("remove oldpkg"), &mut env);
        assert_eq!(state.cart.removals(), &[PkgName::from("oldpkg")]);
    }

    #[test]
    fn add_prints_the_status_line_not_the_table() {
        // A successful stage prints the one-line cart status (header counts +
        // approval standing) — never the numbered table, whose numbers would
        // not be addressable (the quiet-mutation rule).
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        assert!(
            env.lines.contains("transaction — 1 to install"),
            "add should print the status line: {:?}",
            env.lines
        );
        assert!(
            env.lines.contains("needs review"),
            "the status line carries the approval standing: {:?}",
            env.lines
        );
        assert_eq!(env.render_calls.count(), 0, "add must not draw the table");
    }

    #[test]
    fn add_no_op_stays_quiet() {
        // An add that stages nothing (unknown package) must not print status.
        let mut env = FakeEnv::default(); // classifies nothing
        let mut state = State::default();
        state.dispatch(&command::parse("add nope"), &mut env);
        assert!(
            !env.lines.any(|l| l.contains("transaction —")),
            "a no-op add should print no status: {:?}",
            env.lines
        );
    }

    #[test]
    fn drop_prints_the_remaining_status_line() {
        let mut env = env_with(&[("foo", Source::Aur), ("bar", Source::Repo)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo bar"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("drop foo"), &mut env);
        assert!(
            env.lines.contains("transaction — 1 to install"),
            "drop should print the remaining status: {:?}",
            env.lines
        );
        assert_eq!(env.render_calls.count(), 0, "drop must not draw the table");
    }

    #[test]
    fn dropping_the_last_row_reports_the_empty_cart() {
        let mut env = env_with(&[("foo", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add foo"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("drop foo"), &mut env);
        assert!(env.lines.contains("cart is empty"), "{:?}", env.lines);
    }

    #[test]
    fn remove_prints_the_status_with_the_removal_count() {
        let mut env = FakeEnv::default();
        let mut state = State::default();
        state.dispatch(&command::parse("remove oldpkg"), &mut env);
        assert!(
            env.lines
                .contains("transaction — 0 to install, 1 to remove"),
            "remove should print the status line: {:?}",
            env.lines
        );
        assert_eq!(env.render_calls.count(), 0);
    }

    #[test]
    fn approve_surfaces_the_all_approved_moment() {
        // Clearing the last review gate is the "ready to apply" transition —
        // approve prints the status line so the user needn't `show` to learn it.
        let mut env = env_with(&[("a", Source::Aur), ("b", Source::Aur)]);
        let mut state = State::default();
        state.dispatch(&command::parse("add a b"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("approve a"), &mut env);
        assert!(
            env.lines
                .contains("b needs review — run `review b` (or `approve b`)"),
            "a single pending package is named with its command filled in: {:?}",
            env.lines
        );
        env.lines.clear();
        state.dispatch(&command::parse("approve b"), &mut env);
        assert!(
            env.lines.contains("all approved — run `apply`"),
            "the last approval announces readiness: {:?}",
            env.lines
        );
    }

    #[test]
    fn miss_by_search_number_teaches_where_cart_numbers_come_from() {
        // `drop 2` while the search list is the referent, where row 2 isn't
        // staged: the miss names the row, the list, and the package — and
        // points at `show` — instead of a bare "X isn't staged" about a
        // package the user never typed (the incident's confusing half).
        let mut env = env_with(&[("3dslicer", Source::Aur)]);
        env.search_result = vec![
            li_repo("aur", "3dslicer"),     // row 1
            li_repo("aur", "3dslicer-git"), // row 2
        ];
        let mut state = State::default();
        state.dispatch(&command::parse("search 3dslicer"), &mut env);
        state.dispatch(&command::parse("add 1"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("drop 2"), &mut env);
        assert!(
            env.lines.contains(
                "row 2 of the search list (3dslicer-git) isn't staged — `show` numbers the cart"
            ),
            "got: {:?}",
            env.lines
        );
        assert_eq!(cart_specs(&state), vec!["3dslicer"], "nothing was dropped");
    }

    #[test]
    fn drop_by_number_acks_with_the_row() {
        let mut env = FakeEnv {
            upgrade_candidates: vec![up("aur", "bar"), up("aur", "foo")],
            ..FakeEnv::default()
        };
        let mut state = State::default();
        state.dispatch(&command::parse("upgrade"), &mut env); // shows [bar, foo]
        env.lines.clear();
        state.dispatch(&command::parse("drop 2"), &mut env);
        assert!(
            env.lines.contains("dropped foo (row 2)"),
            "the ack confirms the number→package binding: {:?}",
            env.lines
        );
    }

    #[test]
    fn plural_pending_points_at_bare_review() {
        let mut env = env_with(&[("a", Source::Aur), ("b", Source::Aur)]);
        let mut state = State::default();
        env.lines.clear();
        state.dispatch(&command::parse("add a b"), &mut env);
        assert!(
            env.lines
                .contains("2 packages need review — run `review` to walk them"),
            "the plural hint offers the walk-everything command: {:?}",
            env.lines
        );
    }

    #[test]
    fn the_incident_show_then_drop_by_number_hits_the_shown_row() {
        // The motivating transcript: search, `add 1`, then dropping the other
        // staged package by number. `add` prints no numbers, so the search
        // rows stay addressable; `show` renumbers to the cart; `drop 2` then
        // hits exactly the printed cart row.
        let mut env = env_with(&[("3dslicer", Source::Aur), ("3dslicer-bin", Source::Aur)]);
        env.search_result = vec![
            li_repo("aur", "3dslicer"),     // row 1
            li_repo("aur", "3dslicer-git"), // row 2
        ];
        let mut state = State::default();
        state.dispatch(&command::parse("add 3dslicer-bin"), &mut env);
        state.dispatch(&command::parse("search 3dslicer"), &mut env);
        state.dispatch(&command::parse("add 1"), &mut env); // cart: [3dslicer, 3dslicer-bin]
        state.dispatch(&command::parse("show"), &mut env);
        env.lines.clear();
        state.dispatch(&command::parse("drop 2"), &mut env);
        assert!(
            env.lines.contains("dropped 3dslicer-bin"),
            "`drop 2` = shown cart row 2: {:?}",
            env.lines
        );
        assert_eq!(cart_specs(&state), vec!["3dslicer"]);
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
}
