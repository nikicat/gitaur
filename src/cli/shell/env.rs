//! The production [`ShellEnv`]: real I/O against the mirror, index, alpm, and
//! the build pipeline, plus the per-session name caches. The staged
//! transaction is resolved once at `add` ([`RealEnv::stage_plan`]) and frozen
//! in the cart; `show` renders that frozen plan and `apply` executes it, with
//! no re-resolution here.

use super::cart::{
    ApplyOutcome, ApplyRun, Approval, AurApproval, Cart, CartItem, ReviewOutcome, Source,
    StageClass,
};
use super::resolved::{FrozenPreflight, PreflightGate, ResolvedCart};
use super::upgrade;
use super::{ListItem, ShellEnv, State};
use crate::build::reviews::ReviewStore;
use crate::build::{self, ConfirmGate, DevelPolicy, InstallOpts, review};
use crate::cli::dispatch;
use crate::cli::search::{Row, rank_rows};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::index::info::{self, InfoLookup};
use crate::index::{self, AurIndexData, IndexEntry};
use crate::mirror::{self, MirrorRepo};
use crate::names::{PkgBase, PkgName, PkgTarget, RepoName, SearchTerm};
use crate::pacman::alpm_db::{self, PacmanIndex};
use crate::pacman::invoke::{self, PkgUpgrade, REPO_AUR};
use crate::pacman::preflight;
use crate::paths;
use crate::resolver::{Plan, conflict};
use crate::system;
use crate::ui::{self, UpgradeSelection};
use crate::units::ByteSize;
use crate::version::Version;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, SystemTime};
use tracing::debug;

/// The per-session name caches, built once at startup in a single alpm pass.
pub(super) struct NameCaches {
    /// Sorted, de-duplicated — every AUR pkgname + pkgbase from the index plus
    /// sync-repo names, each as a [`PkgTarget`] (the universe a user can name).
    /// Backs glob resolution and tab-completion. An `Rc<[_]>` so the rustyline
    /// completer shares it without copying ~100k names, and a re-`build` on
    /// `upgrade`/`refresh` just swaps the pointer.
    pub(super) universe: Rc<[PkgTarget]>,
    /// Sync-repo pkgname → its repo (`core`, `extra`, …), for `add`'s coarse
    /// repo/AUR classification and the concrete repo column. The first sync DB
    /// (pacman.conf order) that declares a name wins, matching what pacman would
    /// pull.
    pub(super) sync: HashMap<PkgName, RepoName>,
}

/// Build the [`NameCaches`] for a session. A missing index or unreadable alpm
/// just yields smaller caches, never an error.
pub(super) fn build_universe(aur_data: &AurIndexData) -> NameCaches {
    let mut universe: Vec<PkgTarget> = Vec::new();
    let by = aur_data.lookup();
    universe.extend(by.by_name.keys().map(PkgTarget::from));
    universe.extend(by.by_pkgbase.keys().map(PkgTarget::from));
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
pub(super) fn cart_targets(state: &State) -> Vec<PkgTarget> {
    state
        .cart
        .items()
        .iter()
        .map(|it| it.spec().clone())
        .collect()
}

/// Production [`ShellEnv`]: the loaded AUR data + stdout, bridging `upgrade` to
/// the existing loop.
pub(super) struct RealEnv<'a> {
    pub(super) cfg: &'a Config,
    pub(super) devel: DevelPolicy,
    /// The loaded AUR data — *empty* (never absent) when the AUR isn't in
    /// play, so every command takes one uniform path. See
    /// [`AurIndexData::load`].
    pub(super) aur_data: AurIndexData,
    /// Wording-only snapshot of why the session might be empty (banner,
    /// hints). Never drives data flow.
    pub(super) aur_state: index::AurState,
    pub(super) caches: NameCaches,
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
        let outcome = self.reload(upgrade::FetchPolicy::WhenStale)?;
        // `upgrade` never bootstraps (the user's launch-time "later" stands),
        // so its fetch skips an unsynced AUR — degrade to repo-only below,
        // but say so: a silent half answer reads as "nothing to upgrade in
        // the AUR". (`Disabled` is the user's standing choice: no note.)
        if let Some(mirror::RefreshOutcome::AurSkipped(
            mirror::SkipCause::NotSetUp
            | mirror::SkipCause::Declined
            | mirror::SkipCause::NonInteractive,
        )) = outcome
        {
            ui::note("AUR not synced — upgrades are repo-only; `refresh aur` syncs it");
        }
        // With empty AUR data the recompute naturally yields repo upgrades
        // only — no separate fallback path.
        self.aur_data.recompute_remaining(self.devel)
    }

    fn refresh(&mut self, scope: mirror::RefreshScope) -> Result<mirror::RefreshOutcome> {
        // `refresh` is the always-fetch command — it ignores the TTL, so the
        // reload always carries an outcome.
        Ok(self
            .reload(upgrade::FetchPolicy::Refresh(scope))?
            .unwrap_or(mirror::RefreshOutcome::Refreshed))
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
        let aur = self
            .aur_data
            .lookup()
            .search(self.aur_data.index(), &regexes);
        rows.extend(aur.into_iter().map(Row::Aur));
        // One clock + thresholds for the whole render: ranking (the health
        // weight) and the freshness badges classify AUR ages against the same
        // `scale`.
        let scale = ui::AgeScale::now(self.cfg.age_thresholds());
        // Rank the merged repo+AUR list best-first (the `MatchTier` ladder; an
        // abandoned AUR row sinks within its tier; shorter names win; AUR ties
        // break freshest-first). `State::search` prints this reversed, so row 1
        // — the best match — lands right above the prompt.
        let ranked = rank_rows(rows, &regexes, &scale);

        // Resolve installed state + versions against the live pacman DBs and
        // render the aligned table (installed rows emphasized, the installed
        // version styled when behind, freshness band per AUR row). `pac` backs
        // the installed-state lookup in `search_row`; the match-site annotation
        // renders in the printed table; the remembered list carries only
        // selector data.
        let pac = upgrade::system_pac()?;
        let search_rows: Vec<ui::SearchRow> =
            ranked.iter().map(|r| r.search_row(&pac, &scale)).collect();
        // Render best-first rows (row 1 = best) into the configured layout; the
        // list itself prints best-last (bottom-up), so the strongest matches
        // land next to the prompt with the low, easy-to-type numbers. Numbers
        // still key the best-first `items` returned below, so `add 1` is always
        // the top match regardless of print direction.
        let table = ui::SearchList {
            rows: &search_rows,
            numbers: ui::RowNumbers::Numbered,
            layout: self.cfg.search_layout,
        }
        .render(ui::Paint::detect(), ui::term_width());
        let items = ranked
            .iter()
            .map(|r| ListItem {
                target: r.row.picked(),
                repo: Some(RepoName::from(r.row.repo_name())),
            })
            .collect();
        // `ranked` borrows the session's AUR data, so it must be done before
        // printing takes `&mut self`.
        drop(ranked);
        self.print_table(&table);
        Ok(items)
    }

    fn show_info(&mut self, targets: &[PkgTarget]) -> Result<()> {
        // The shared repo-then-AUR lookup (`-Si` runs the same one); only the
        // miss wording is the shell's — its sync verb is `refresh aur`.
        let missing = InfoLookup::open(&self.aur_data)?.print_all(targets);
        if !missing.is_empty() {
            ui::warn(&info::missing_warning(
                self.aur_state,
                &missing,
                "`refresh aur`",
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
        self.aur_data.entry(target).map(|_| StageClass {
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

    fn aur_state(&self) -> index::AurState {
        self.aur_state
    }

    fn pkgbase_of(&self, target: &PkgTarget) -> Option<PkgBase> {
        self.aur_data.entry(target).map(|e| e.pkgbase.clone())
    }

    fn previously_approved(&self, target: &PkgTarget) -> bool {
        let Some(entry) = self.aur_data.entry(target) else {
            return false;
        };
        // The index entry carries the mirror commit it was built from — the
        // same identity `prepare_one` gates on — so no mirror open is needed.
        // Any store trouble is a miss: the worst case is one extra review.
        match ReviewStore::open(&paths::reviews_db_path()) {
            Ok(store) => store
                .approved(&entry.pkgbase, &gix::ObjectId::from(entry.commit_oid))
                .unwrap_or_else(|e| {
                    debug!(pkgbase = %entry.pkgbase, error = %e, "review store read failed");
                    false
                }),
            Err(e) => {
                debug!(error = %e, "review store unavailable");
                false
            }
        }
    }

    fn record_approval(&mut self, target: &PkgTarget) {
        let Some(entry) = self.aur_data.entry(target) else {
            return;
        };
        let saved = ReviewStore::open(&paths::reviews_db_path()).and_then(|store| {
            store.record_approval(
                &entry.pkgbase,
                &gix::ObjectId::from(entry.commit_oid),
                &entry.version(),
            )
        });
        if let Err(e) = saved {
            ui::warn(&format!("could not save review approval: {e}"));
        }
    }

    fn review(&mut self, target: &PkgTarget) -> Result<ReviewOutcome> {
        let aur_data = &self.aur_data;
        let Some(entry) = aur_data.entry(target) else {
            ui::warn(&format!("{}: not an AUR package", target.as_str()));
            return Ok(ReviewOutcome::Skipped);
        };
        let pkgbase = entry.pkgbase.clone();
        let new_ver = entry.version();

        // Materialise the worktree + resolve the installed counterpart (the
        // installed package this build will replace) exactly like
        // `build::prepare_one`, so the diff base and review header match what
        // `apply` would show. The hint matters: when the target names a
        // pkgname or provides entry rather than the pkgbase, that name says
        // WHICH installed package the user means. Without it the lookup just
        // takes the first installed name from the provides list, and the
        // header labels the wrong package. Same rule `expand_pkgbase_targets`
        // applies when recording hints for apply.
        let mirror = MirrorRepo::open(&paths::aur_repo_path())?;
        let wt = mirror::worktree::add_or_reset(&mirror, &pkgbase, &paths::pkg_worktree(&pkgbase))?;
        let alpm = alpm_db::open()?;
        let pac = PacmanIndex::build(&alpm);
        let hint = review_hint(&pkgbase, target);
        let counterpart = pac.counterpart_with_hint(entry, hint.as_ref());

        let request = review::ReviewRequest {
            mirror: &mirror,
            pkgbase: &pkgbase,
            new_ver: &new_ver,
            counterpart: counterpart.as_ref(),
            wt: &wt,
            history_scan_max: self.cfg.review_history_scan_max,
        };
        // The shell drives one interactive review per call; the "approve
        // all" fast path is the dispatch loop's job (it decides whether to
        // call this at all), so a single review always prompts.
        match request.review(review::Prompting::Prompt) {
            Ok(review::Outcome::Approved) => Ok(ReviewOutcome::Approved),
            Ok(review::Outcome::ApprovedAll) => Ok(ReviewOutcome::ApprovedAll),
            Ok(review::Outcome::Skipped) => Ok(ReviewOutcome::Skipped),
            // An abort at the review prompt ends the pass but not the shell.
            Err(Error::UserAbort) => Ok(ReviewOutcome::Aborted),
            Err(e) => Err(e),
        }
    }

    fn render_cart(&mut self, cart: &Cart) {
        // Render the unified change-set table straight from the cart's frozen
        // resolution (roots + pulled-in deps + removals + cost) — no resolve
        // here; the plan was frozen at `add`. A non-empty cart always carries a
        // resolution (every install-set change re-resolves or rejects), so the
        // `None` arm is just defensive: fall back to the flat staged rows.
        let Some(resolved) = cart.resolution() else {
            self.print_table(&flat_cart_lines(cart, &Error::other("cart not resolved")));
            return;
        };
        let table = resolved_table(cart, &self.aur_data, resolved);
        self.print_table(&table);
        // The sysupgrade preflight verdict for the staged repo lane — "upgrading
        // X breaks Y" plus the shell-native way out — belongs on this same
        // screen, ahead of any `apply`.
        for note in &resolved.preflight.notes {
            upgrade::print_preflight_note(note);
        }
    }

    fn apply(&mut self, cart: &Cart) -> Result<ApplyRun> {
        // Execute the transaction frozen at `add`/`upgrade` — **no resolution
        // here**. The run's review scratch: seeded from the cart, extended by
        // any PKGBUILD reviewed mid-run (pulled-in AUR deps), and carried back
        // to the dispatch core in the ApplyRun — on every outcome, so those
        // approvals survive a failed run's retry.
        let mut reviewed = cart.reviewed().clone();
        let aur_data = &self.aur_data;
        let pac = upgrade::system_pac()?;
        // A non-empty, all-approved cart always carries a resolution (every
        // install-set change re-resolved or rejected).
        let Some(resolved) = cart.resolution() else {
            return Err(Error::other(
                "nothing resolved — re-`add` to rebuild the transaction",
            ));
        };
        let blocker_plan = resolved.blocker_plan.as_ref();
        let main_plan = resolved.main_plan.as_ref();
        let repo_sel = &resolved.repo_sel;

        // Sysupgrade gate — **consumption only**: the breakage was detected at
        // `add` (frozen notes + the blocker/blocking split). When unresolved
        // breakage remains, print the notes for context and ask the override —
        // the synced snapshot is advisory, so walking away must mean no. (The
        // notes for a *resolved* breakage already showed at `show`.)
        if resolved.preflight.gate == PreflightGate::NeedsOverride {
            for note in &resolved.preflight.notes {
                upgrade::print_preflight_note(note);
            }
            if !ui::confirm_default_no("Repo upgrade expected to fail — run pacman anyway?")
                .map_err(|e| Error::other(format!("confirm: {e}")))?
            {
                return Ok(ApplyRun {
                    outcome: ApplyOutcome::Declined,
                    reviewed,
                });
            }
        }

        // No table redraw — `show` is where the user looked. No confirm either:
        // the typed `apply` after the approval gate *is* the informed consent
        // (consent at a decision point — don't double-prompt an explicit
        // command). The one-line cost summary prints as a receipt of what the
        // run is about to do.
        let roots = txn_roots(cart, aur_data, &resolved.size_pac);
        let (repo_deps, aur_deps) = merged_dep_rows(main_plan, blocker_plan);
        let removals: Vec<PkgName> = cart.removals().to_vec();
        let metrics = upgrade::preview_metrics(aur_data, &roots, main_plan);
        ui::info(
            &ui::ChangeSet {
                roots: &roots,
                repo_deps: &repo_deps,
                aur_deps: &aur_deps,
                removals: &removals,
                pac: &resolved.size_pac,
                metrics: &metrics,
            }
            .summary(),
        );

        let opts = InstallOpts {
            noconfirm: false,
            asdeps: false,
            gate: ConfirmGate::AlreadyConfirmed,
        };
        let ctx = build::InstallCtx {
            cfg: self.cfg,
            aur: aur_data,
            pac: &pac,
        };

        // Blocker rebuilds first — installing them is what unblocks the repo
        // lane (the rebuilt packages no longer carry the dependency the
        // sysupgrade would break).
        let mut report = build::RunReport::default();
        if let Some(plan) = blocker_plan {
            report = ctx.apply_plan(plan, opts, &mut reviewed)?;
            if !report.all_landed() {
                // The blocker didn't land, so the repo lane would fail exactly
                // as preflighted — stop before it runs. The repo rows haven't
                // run either, so they stay staged (`repo_landed = false`).
                return Ok(ApplyRun {
                    outcome: cart_apply_outcome(&report, cart, aur_data, false),
                    reviewed,
                });
            }
        }

        // Repo upgrades next (before the main AUR builds, so those link against
        // the upgraded libs), via a partial `pacman -Su` against the rootless
        // synced db that ignores every repo candidate the user didn't stage.
        if !repo_sel.repo.is_empty() {
            dispatch::run_repo_upgrade(self.cfg, repo_sel)?;
        }

        // Build + install the main AUR (and any fresh-install) half. The
        // explicit `apply` was the consent, so `apply_plan` doesn't re-ask.
        if let Some(plan) = main_plan {
            let main_report = ctx.apply_plan(plan, opts, &mut reviewed)?;
            report.absorb(main_report);
        }
        let outcome = cart_apply_outcome(&report, cart, aur_data, true);
        if !matches!(outcome, ApplyOutcome::Succeeded) {
            return Ok(ApplyRun { outcome, reviewed });
        }

        // Remove half: `pacman -R`, filtered to packages actually installed so a
        // retry after a partial failure doesn't trip on an already-gone target.
        // One atomic add+remove transaction is the phase-6 native-commit goal;
        // until then this is separate transactions bridged by the sudo cache.
        let installed_removals: Vec<&PkgName> = cart
            .removals()
            .iter()
            .filter(|n| pac.is_installed(n))
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
                let installed = cart.items().iter().map(|it| it.spec().clone()).collect();
                return Ok(ApplyRun {
                    outcome: ApplyOutcome::Failed { installed },
                    reviewed,
                });
            }
        }
        Ok(ApplyRun {
            outcome: ApplyOutcome::Succeeded,
            reviewed,
        })
    }

    fn stage_plan(&self, cart: &Cart) -> Result<ResolvedCart> {
        let aur_data = &self.aur_data;
        let pac = upgrade::system_pac()?;
        // Seed the split-package picker from the cart's *existing* resolution
        // (this add hasn't replaced it yet): an already-resolved split root
        // returns its stored subset without re-opening its picker. A new split
        // root has no seed entry and prompts once.
        let seed = cart
            .resolution()
            .map(|r| merged_selections(r))
            .unwrap_or_default();

        // The repo-upgrade lane + its sysupgrade preflight only matter when repo
        // upgrades are staged — skip the candidate recompute otherwise.
        let (repo_sel, preflight) = if cart.repo_upgrades().is_empty() {
            (UpgradeSelection::default(), FrozenPreflight::default())
        } else {
            let sel = self.repo_upgrade_selection(aur_data, cart)?;
            let pf = frozen_preflight(aur_data, cart, &sel);
            (sel, pf)
        };

        // Resolve the build/install half (AUR + fresh installs) as two plans:
        // sysupgrade blockers — staged rebuilds whose install unblocks the repo
        // lane — run first, so they resolve separately from the main half. Repo
        // *upgrades* take the partial `-Su` lane, never the plan.
        let (blocker_targets, main_targets): (Vec<build::Target>, Vec<build::Target>) = cart
            .install_targets()
            .into_iter()
            .partition(|t| preflight.blockers.iter().any(|b| b == t.spec.as_str()));
        let blocker_plan = self.resolve_seeded(aur_data, &pac, &blocker_targets, &seed)?;
        let main_plan = self.resolve_seeded(aur_data, &pac, &main_targets, &seed)?;

        // Reject a declared conflict before the cart keeps the plan: a staged AUR
        // package that would collide with another staged or installed package
        // (and isn't a transparent `replaces=` swap) fails the `add` here rather
        // than at `apply`, after the build. `?` propagates the reject.
        let mut staged_names: HashSet<PkgName> = HashSet::new();
        let mut declarers: Vec<conflict::Declarer> = Vec::new();
        plan_conflict_inputs(
            blocker_plan.as_ref(),
            aur_data,
            &mut staged_names,
            &mut declarers,
        );
        plan_conflict_inputs(
            main_plan.as_ref(),
            aur_data,
            &mut staged_names,
            &mut declarers,
        );
        let removing: HashSet<PkgName> = cart.removals().iter().cloned().collect();
        conflict::check(
            &declarers,
            &staged_names,
            |n| pac.installed.contains_key(n),
            &removing,
        )?;

        // Sizes/versions from the freshly-synced db (the new versions' real
        // download cost), frozen so `show`/`apply` don't reopen alpm.
        let size_pac = upgrade::synced_pac()?;
        Ok(ResolvedCart {
            blocker_plan,
            main_plan,
            repo_sel,
            preflight,
            size_pac,
        })
    }

    fn system_usage(&mut self) -> system::Report {
        system::usage()
    }

    fn system_prune(&mut self) -> Result<Option<ByteSize>> {
        // Quote what the deletion is worth before asking — the mirror alone is
        // multi-GiB and minutes of re-fetch, so the user should decline cheap.
        let would_free = system::usage().prunable_total();
        let prompt =
            format!("Delete all caches — AUR mirror, index, sync dbs, build trees ({would_free})?");
        if !ui::confirm_default_no(&prompt)? {
            return Ok(None);
        }
        // The in-memory AUR data (mmap of the now-deleted index) stays valid
        // and loaded on purpose: search/info keep answering from it, and the
        // next `refresh aur` re-bootstraps the mirror + index from scratch
        // (the bare `refresh`/`upgrade` never spring the re-clone).
        system::prune().map(Some)
    }
}

impl RealEnv<'_> {
    /// Re-fetch the mirror + index (subject to `policy`'s TTL) and reload the
    /// session in place, rebuilding the name caches so fresh data backs
    /// subsequent `search`/`info`/classification + completion. Shared by
    /// `upgrade` (which then recomputes candidates) and `refresh` (which stops
    /// here); both consume the returned outcome (`None` when the TTL skipped
    /// the fetch) to word what the refresh actually did. Invalidates the `show`
    /// resolution cache — the mirror/db data it was resolved against may have
    /// just changed.
    fn reload(&mut self, policy: upgrade::FetchPolicy) -> Result<Option<mirror::RefreshOutcome>> {
        let reload = upgrade::refresh_and_reload(self.cfg, policy)?;
        self.caches = build_universe(&reload.data);
        self.aur_data = reload.data;
        self.aur_state = index::AurState::probe(self.cfg);
        Ok(reload.outcome)
    }

    /// Resolve `targets` (a phase-subset of the cart's install/build half) into
    /// a [`Plan`], reusing `seed`'s split-package choices so an already-resolved
    /// split root doesn't re-prompt. `None` when the subset is empty (a
    /// repo-upgrade-only or removal-only cart / phase). Repo *upgrades* take the
    /// `-Su` lane, so they are never targets here.
    fn resolve_seeded(
        &self,
        aur_data: &AurIndexData,
        pac: &PacmanIndex,
        targets: &[build::Target],
        seed: &HashMap<PkgBase, Vec<PkgName>>,
    ) -> Result<Option<Plan>> {
        if targets.is_empty() {
            return Ok(None);
        }
        let ctx = build::InstallCtx {
            cfg: self.cfg,
            aur: aur_data,
            pac,
        };
        Ok(Some(ctx.resolve_seeded(targets, seed)?))
    }

    /// Turn the staged repo upgrades into the partial-`-Su` selection: the
    /// staged ones are upgraded; every other current repo candidate is
    /// `--ignore`d. Recomputes the candidate set so a stale cart can't pin the
    /// wrong packages.
    fn repo_upgrade_selection(
        &self,
        aur_data: &AurIndexData,
        cart: &Cart,
    ) -> Result<UpgradeSelection> {
        let staged: HashSet<PkgName> = cart
            .repo_upgrades()
            .iter()
            .map(|u| u.name.clone())
            .collect();
        let mut repo = Vec::new();
        let mut repo_skipped = Vec::new();
        for u in aur_data
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
/// Which installed package does the user mean by `review <target>`? When the
/// target names a pkgname or a provides entry (anything other than the
/// pkgbase itself), that name is the answer — return it as the hint for the
/// installed-counterpart lookup, the same rule `expand_pkgbase_targets`
/// applies when recording hints for `apply`. A target naming the pkgbase
/// says nothing more specific, so no hint.
fn review_hint(pkgbase: &PkgBase, target: &PkgTarget) -> Option<PkgName> {
    let name = PkgName::from(target.as_str());
    (!pkgbase.matches_pkgname(&name)).then_some(name)
}

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
    aur_data: &AurIndexData,
    repo_landed: bool,
) -> ApplyOutcome {
    if report.all_landed() {
        return ApplyOutcome::Succeeded;
    }
    let installed = landed_install_specs(cart, &report.installed, repo_landed, |it| {
        aur_data.entry(it.spec()).map(|e| e.pkgbase.clone())
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
        .map(|it| it.spec().clone())
        .collect()
}

/// Run the read-only sysupgrade preflight for the staged repo lane and freeze
/// its verdict at `add` time: the rendered notes plus the blocker/blocking
/// split. **No printing and no prompt** — `show` prints the notes under the
/// table, and `apply` prints them again and asks the override only when
/// unresolved breakage remains ([`FrozenPreflight::blocking`]).
///
/// Best-effort, like every preflight consumer: if the machinery itself can't
/// run, pacman remains the authority and the verdict is empty (no breakage
/// claimed).
fn frozen_preflight(
    aur_data: &AurIndexData,
    cart: &Cart,
    sel: &UpgradeSelection,
) -> FrozenPreflight {
    if sel.repo.is_empty() {
        return FrozenPreflight::default();
    }
    let issues = match preflight::sysupgrade(&sel.repo_skipped) {
        Ok(issues) => issues,
        Err(e) => {
            debug!(error = %e, "sysupgrade preflight skipped (could not run prepare)");
            return FrozenPreflight::default();
        }
    };
    if issues.is_empty() {
        return FrozenPreflight::default();
    }
    // The same structured events the passthrough lane logs, emitted once here
    // (at resolve), then the human-facing notes with remediation.
    preflight::log_issues(&issues);
    let notes = upgrade::preflight_notes(issues, aur_data, cart);
    let mut blockers = Vec::new();
    let mut gate = PreflightGate::Clear;
    for note in &notes {
        match &note.remedy {
            upgrade::Remedy::StagedRebuild { target } => blockers.push(target.clone()),
            _ => gate = PreflightGate::NeedsOverride,
        }
    }
    FrozenPreflight {
        notes,
        blockers,
        gate,
    }
}

/// The split-package selections to seed the picker with when re-resolving the
/// whole cart: the union of the prior resolution's plans' `pkgname_selections`,
/// so an already-resolved split root returns its stored subset without
/// re-prompting. The main plan wins a shared pkgbase (blocker/main are disjoint
/// in practice, so the tie never bites).
fn merged_selections(resolved: &ResolvedCart) -> HashMap<PkgBase, Vec<PkgName>> {
    let mut out: HashMap<PkgBase, Vec<PkgName>> = HashMap::new();
    // Blocker first, then main — so `extend`'s last-wins leaves the main plan's
    // choice for any pkgbase in both (in practice the two are disjoint, so the
    // tie never bites). `extend` rather than a `for` over the maps keeps the
    // merge order-insensitive.
    for plan in [resolved.blocker_plan.as_ref(), resolved.main_plan.as_ref()]
        .into_iter()
        .flatten()
    {
        out.extend(
            plan.pkgname_selections
                .iter()
                .map(|(pb, sel)| (pb.clone(), sel.clone())),
        );
    }
    out
}

/// Gather one plan's contributions to the declared-conflict check: the concrete
/// pkgnames it installs (repo + the selected AUR pkgnames) into `staged`, and an
/// AUR [`conflict::Declarer`] per installed AUR pkgname carrying its pkgbase's
/// `conflicts=`/`replaces=`. Only the *selected* pkgnames of a split package are
/// counted — an unselected sibling isn't installed, so it neither joins the
/// present set nor declares a conflict.
fn plan_conflict_inputs(
    plan: Option<&Plan>,
    aur_data: &AurIndexData,
    staged: &mut HashSet<PkgName>,
    declarers: &mut Vec<conflict::Declarer>,
) {
    let Some(plan) = plan else {
        return;
    };
    for name in plan.direct_repo.iter().chain(&plan.transitive_repo) {
        staged.insert(PkgName::from(name.as_str()));
    }
    let by = aur_data.lookup();
    let idx = aur_data.index();
    for pb in plan.aur_strata.iter().flatten() {
        let Some(entry) = by.lookup_pkgbase(idx, pb) else {
            continue;
        };
        // The selected subset for a split target, else every pkgname the pkgbase
        // produces (the whole-pkgbase default).
        let selected = plan
            .pkgname_selections
            .get(pb)
            .cloned()
            .unwrap_or_else(|| entry.pkgnames.iter().map(|p| p.name.clone()).collect());
        for name in selected {
            staged.insert(name.clone());
            declarers.push(conflict::Declarer {
                name,
                conflicts: entry.conflicts.clone(),
                replaces: entry.replaces.clone(),
            });
        }
    }
}

/// Build the unified change-set table for `show` from the cart's frozen
/// resolution: the approval-bearing root rows re-derived from the live cart
/// (cheap — no I/O, so `approve`/`review` reflect on the next `show` without a
/// re-resolve), the pulled-in dep rows and build overlay derived from the frozen
/// plans, and the frozen synced snapshot for sizes/versions.
fn resolved_table(cart: &Cart, aur_data: &AurIndexData, resolved: &ResolvedCart) -> ui::Table {
    let roots = txn_roots(cart, aur_data, &resolved.size_pac);
    let (repo_deps, aur_deps) =
        merged_dep_rows(resolved.main_plan.as_ref(), resolved.blocker_plan.as_ref());
    let removals: Vec<PkgName> = cart.removals().to_vec();
    let metrics = upgrade::preview_metrics(aur_data, &roots, resolved.main_plan.as_ref());
    ui::ChangeSet {
        roots: &roots,
        repo_deps: &repo_deps,
        aur_deps: &aur_deps,
        removals: &removals,
        pac: &resolved.size_pac,
        metrics: &metrics,
    }
    .table(ui::Paint::detect())
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
fn txn_roots(cart: &Cart, aur_data: &AurIndexData, size_pac: &PacmanIndex) -> Vec<ui::TxnRoot> {
    let now = SystemTime::now();
    cart.items()
        .iter()
        .map(|it| {
            let (old_ver, new_ver) = row_versions(it, aur_data, size_pac);
            ui::TxnRoot {
                repo: it.repo_label(),
                approval: approval_cell(it.approval),
                name: PkgName::from(it.spec().as_str()),
                old_ver,
                new_ver,
                age: aur_age(it, aur_data, now),
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
    aur_data: &AurIndexData,
    size_pac: &PacmanIndex,
) -> (Option<Version>, Option<Version>) {
    if let Some(u) = &it.upgrade {
        return (Some(u.old_ver.clone()), Some(u.new_ver.clone()));
    }
    let new = match it.source {
        Source::Aur => aur_data.entry(it.spec()).map(IndexEntry::version),
        Source::Repo => size_pac.sync_version(it.spec().as_str()).map(Version::from),
    };
    (None, new)
}

/// The AUR pkgbase's "last modified" age (its branch-tip commit time vs `now`),
/// for the table's age column. `None` for repo rows, when there's no matching
/// index entry, or when the commit time is unrecorded (the
/// [`crate::units::UnixTime`] sentinel in archives predating the field).
fn aur_age(it: &CartItem, aur_data: &AurIndexData, now: SystemTime) -> Option<Duration> {
    if it.source != Source::Aur {
        return None;
    }
    let entry = aur_data.entry(it.spec())?;
    now.duration_since(entry.commit_time.system_time()?).ok()
}

/// The graceful-degradation rendering for `show` when the resolve behind the
/// unified table fails (unknown target, mirror gap): a note plus the flat staged
/// rows, so `show` still tells the user what's in the cart instead of erroring.
/// Laid out by [`ui::Grid`] with the version transition as the row tail; the
/// removal rows keep their own literal shape beneath.
fn flat_cart_lines(cart: &Cart, err: &Error) -> ui::Table {
    let mut out = ui::Table::new();
    out.push(format!(
        "  (couldn't resolve the full change set: {err} — showing staged items)"
    ));
    let mut grid = ui::Grid::new(vec![
        ui::Col::right().min(ui::Width::of("999")), // № — the {:>3} it replaces
        ui::Col::left(),                            // repo label
        ui::Col::left(),                            // approval
        ui::Col::left(),                            // spec
    ]);
    for (i, it) in cart.items().iter().enumerate() {
        // The `old → new` transition rides as an unaligned tail cell (the grid
        // supplies the gap); a fresh install has none.
        let ver = it
            .version_transition()
            .map_or_else(|| ui::Cell::plain(""), ui::Cell::plain);
        grid.push(
            ui::GridRow::new(vec![
                ui::Cell::plain((i + 1).to_string()),
                ui::Cell::plain(it.repo_label().to_string()),
                ui::Cell::plain(it.approval.label()),
                ui::Cell::plain(it.spec().as_str()),
            ])
            .tail(vec![ver]),
        );
    }
    out.append(grid.render());
    for name in cart.removals() {
        out.push(format!("     remove  {name}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cli::shell::testenv::up;

    /// The flat fallback keeps `show` alive when the resolve fails: the note
    /// names the error, every staged item renders as an aligned numbered row,
    /// and removals list beneath. Nothing else pins this shape — it only
    /// renders on a resolve failure.
    #[test]
    fn flat_cart_lines_note_rows_and_removals() {
        use super::super::cart::{AurApproval, Source};
        use crate::{assert_contains, assert_regex};

        let mut cart = Cart::default();
        cart.add(CartItem::new(
            PkgTarget::new("some-aur-thing"),
            Source::Aur,
            None,
            AurApproval::Review,
        ));
        cart.stage_remove(PkgName::from("old-cruft"));

        let table = flat_cart_lines(&cart, &Error::other("mirror gap"));
        let lines = table.lines();
        assert_contains!(lines[0], "couldn't resolve the full change set");
        assert_contains!(lines[0], "mirror gap");
        assert_regex!(lines[1], r"^  1  aur\s+review\s+some-aur-thing$");
        assert_eq!(lines[2], "     remove  old-cruft");
    }

    /// A `review` target that names a pkgname/provides becomes the hint —
    /// it says which installed package the user means. One naming the
    /// pkgbase itself yields none. Without the hint,
    /// `counterpart_with_hint` takes the first installed name from the
    /// provides list, and the review header labels the wrong package.
    #[test]
    fn review_hint_carries_foreign_name_not_pkgbase() {
        let pkgbase = PkgBase::from("test-syu-hint-new");
        let foreign = PkgTarget::new("test-syu-hint-older");
        assert_eq!(
            review_hint(&pkgbase, &foreign),
            Some(PkgName::from("test-syu-hint-older"))
        );
        let canonical = PkgTarget::new("test-syu-hint-new");
        assert_eq!(review_hint(&pkgbase, &canonical), None);
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
        let pkgbase_of = |it: &CartItem| Some(PkgBase::from(it.spec().as_str()));
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
}
