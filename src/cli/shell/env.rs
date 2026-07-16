//! The production [`ShellEnv`]: real I/O against the mirror, index, alpm, and
//! the build pipeline, plus the per-session name caches and the cached
//! transaction view that `show` and `apply` share.

use super::cart::{
    ApplyOutcome, Approval, AurApproval, Cart, CartItem, ReviewOutcome, Source, StageClass,
};
use super::upgrade;
use super::{ListItem, ShellEnv, State};
use crate::build::{self, ConfirmGate, DevelPolicy, InstallOpts, review};
use crate::cli::dispatch;
use crate::cli::search::{Row, rank_rows, search_row};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::index::info::{self, InfoLookup};
use crate::index::{self, AurIndexData, IndexEntry};
use crate::mirror::{self, MirrorRepo};
use crate::names::{PkgBase, PkgName, PkgTarget, RepoName, RepoRank, SearchTerm};
use crate::pacman::alpm_db::{self, PacmanIndex};
use crate::pacman::invoke::{self, PkgUpgrade, REPO_AUR};
use crate::pacman::preflight;
use crate::paths;
use crate::resolver::Plan;
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
        .map(|it| PkgTarget::new(it.spec()))
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
    /// Cached resolution of the cart's package set for `show` — see
    /// [`CachedTxn`]. `None` until the first render, after a reload, or after an
    /// `apply` (which may have changed the installed set).
    pub(super) view: Option<CachedTxn>,
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
pub(super) struct CachedTxn {
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
        self.aur_data
            .lookup()
            .lookup(self.aur_data.index(), target.as_str())
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

    fn aur_state(&self) -> index::AurState {
        self.aur_state
    }

    fn pkgbase_of(&self, target: &PkgTarget) -> Option<PkgBase> {
        self.aur_data
            .lookup()
            .lookup(self.aur_data.index(), target.as_str())
            .map(|e| e.pkgbase.clone())
    }

    fn review(&mut self, target: &PkgTarget) -> Result<ReviewOutcome> {
        let aur_data = &self.aur_data;
        let Some(entry) = aur_data.entry(target.as_str()) else {
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
        let aur_data = &self.aur_data;
        let pac = upgrade::system_pac()?;

        // Preflight the repo-upgrade lane before anything is asked of the user:
        // re-sync the rootless db (the drift guard — the check should see what
        // `pacman -Syu`'s own `-Sy` is about to fetch), recompute the
        // partial-upgrade selection against it, and simulate the `-Su`. Failing
        // that check ends the apply here — before the cost summary and the
        // sudo prompt.
        if !cart.repo_upgrades().is_empty() {
            upgrade::resync_repo_dbs(self.cfg);
        }
        let repo_sel = self.repo_upgrade_selection(aur_data, cart)?;
        let blockers = if repo_sel.repo.is_empty() {
            Vec::new()
        } else {
            match sysupgrade_gate(aur_data, cart, &repo_sel)? {
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
        let blocker_plan = self.resolve_plan(aur_data, &pac, &blocker_targets)?;
        let main_plan = self.resolve_plan(aur_data, &pac, &main_targets)?;

        // No table redraw — `show` is where the user looked. No confirm either:
        // the typed `apply` after the approval gate *is* the informed consent
        // (consent at a decision point — don't double-prompt an explicit
        // command). The one-line cost summary prints as a receipt of what the
        // run is about to do.
        let size_pac = upgrade::synced_pac()?;
        let roots = txn_roots(cart, aur_data, &size_pac);
        let (repo_deps, aur_deps) = merged_dep_rows(main_plan.as_ref(), blocker_plan.as_ref());
        let removals: Vec<PkgName> = cart.removals().to_vec();
        let metrics = upgrade::preview_metrics(aur_data, &roots, main_plan.as_ref());
        ui::info(
            &ui::ChangeSet {
                roots: &roots,
                repo_deps: &repo_deps,
                aur_deps: &aur_deps,
                removals: &removals,
                pac: &size_pac,
                metrics: &metrics,
            }
            .summary(),
        );

        let mut reviewed = cart.reviewed().clone();
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
        if let Some(plan) = &blocker_plan {
            report = ctx.apply_plan(plan, opts, &mut reviewed)?;
            if !report.all_landed() {
                // The blocker didn't land, so the repo lane would fail exactly
                // as preflighted — stop before it runs. The repo rows haven't
                // run either, so they stay staged (`repo_landed = false`).
                return Ok(cart_apply_outcome(&report, cart, aur_data, false));
            }
        }

        // Repo upgrades next (before the main AUR builds, so those link against
        // the upgraded libs), via a partial `pacman -Syu` that ignores every
        // repo candidate the user didn't stage.
        if !repo_sel.repo.is_empty() {
            dispatch::run_repo_upgrade(self.cfg, &repo_sel)?;
        }

        // Build + install the main AUR (and any fresh-install) half. The
        // explicit `apply` was the consent, so `apply_plan` doesn't re-ask.
        if let Some(plan) = &main_plan {
            let main_report = ctx.apply_plan(plan, opts, &mut reviewed)?;
            report.absorb(main_report);
        }
        let outcome = cart_apply_outcome(&report, cart, aur_data, true);
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
    /// Build the build-time overlay for the search table from the metrics store —
    /// only the **installed AUR** rows (build time is a property we show for
    /// installed packages, keyed by pkgname). Empty when there's no session or no
    /// such rows, in which case the table's build column stays blank.
    fn search_metrics(&self, rows: &[ui::SearchRow]) -> ui::PreviewMetrics {
        let aur_data = &self.aur_data;
        let roots: Vec<ui::TxnRoot> = rows
            .iter()
            .filter(|r| r.install.installed() && r.repo.rank() == RepoRank::Aur)
            .map(|r| ui::TxnRoot {
                repo: r.repo.clone(),
                approval: ui::ApprovalCell::Approved,
                name: r.name.clone(),
                old_ver: r.upgrade_from().cloned(),
                new_ver: r.new_ver.clone(),
                age: None,
            })
            .collect();
        if roots.is_empty() {
            return ui::PreviewMetrics::empty();
        }
        upgrade::preview_metrics(aur_data, &roots, None)
    }

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
        self.view = None;
        Ok(reload.outcome)
    }

    /// Build the unified change-set table for `show` from the cached resolution,
    /// re-deriving the approval-bearing root rows from the live cart each call
    /// (cheap — no I/O). [`ui::transaction_table`] returns an owned [`ui::Table`]
    /// (it holds no borrow of the cache), so `render_cart` can print it after the
    /// borrow ends; errors bubble so the caller can fall back to the flat staged
    /// rows — `show` must never abort.
    fn transaction_view(&mut self, cart: &Cart) -> Result<ui::Table> {
        self.ensure_view(cart)?;
        let aur_data = &self.aur_data;
        let r = &self
            .view
            .as_ref()
            .expect("ensure_view populated the cache on Ok")
            .resolved;
        let roots = txn_roots(cart, aur_data, &r.size_pac);
        let removals: Vec<PkgName> = cart.removals().to_vec();
        Ok(ui::ChangeSet {
            roots: &roots,
            repo_deps: &r.repo_deps,
            aur_deps: &r.aur_deps,
            removals: &removals,
            pac: &r.size_pac,
            metrics: &r.metrics,
        }
        .table(ui::Paint::detect()))
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
        let aur_data = &self.aur_data;
        let pac = upgrade::system_pac()?;
        let plan = self.resolve_plan(aur_data, &pac, &cart.install_targets())?;
        // Sizes from the freshly-synced db (the new versions' real download cost).
        let size_pac = upgrade::synced_pac()?;
        let (repo_deps, aur_deps) = upgrade::dep_rows(plan.as_ref());
        // Roots feed only the (approval-independent) build-time overlay here; the
        // render re-derives approval-aware roots from the live cart.
        let roots = txn_roots(cart, aur_data, &size_pac);
        let metrics = upgrade::preview_metrics(aur_data, &roots, plan.as_ref());
        let preflight = self.preview_preflight(aur_data, cart);
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
        aur_data: &AurIndexData,
        cart: &Cart,
    ) -> Vec<upgrade::PreflightNote> {
        if cart.repo_upgrades().is_empty() {
            return Vec::new();
        }
        let sel = match self.repo_upgrade_selection(aur_data, cart) {
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
            Ok(issues) => upgrade::preflight_notes(issues, aur_data, cart),
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
        aur_data: &AurIndexData,
        pac: &PacmanIndex,
        targets: &[build::Target],
    ) -> Result<Option<Plan>> {
        if targets.is_empty() {
            return Ok(None);
        }
        let ctx = build::InstallCtx {
            cfg: self.cfg,
            aur: aur_data,
            pac,
        };
        Ok(Some(ctx.resolve_targets(targets, false)?))
    }

    /// Turn the staged repo upgrades into the partial-`-Syu` selection: the
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
    aur_data: &AurIndexData,
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
    let notes = upgrade::preflight_notes(issues, aur_data, cart);
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
fn txn_roots(cart: &Cart, aur_data: &AurIndexData, size_pac: &PacmanIndex) -> Vec<ui::TxnRoot> {
    let now = SystemTime::now();
    cart.items()
        .iter()
        .map(|it| {
            let (old_ver, new_ver) = row_versions(it, aur_data, size_pac);
            ui::TxnRoot {
                repo: it.repo_label(),
                approval: approval_cell(it.approval),
                name: PkgName::from(it.spec()),
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
        Source::Repo => size_pac.sync_version(it.spec()).map(Version::from),
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

    use crate::cli::shell::testenv::up;

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
}
