//! The no-arg `gaur` upgrade loop.
//!
//! Unlike the single-shot `-Syu` path (`super::dispatch::handle_s`), the
//! interactive no-arg run is iterative: refresh the mirror **once**, then loop
//! over recompute → picker → apply until the user is done. The expensive
//! once-per-session work (mirror fetch, AUR index load) is hoisted out of the
//! iteration; only the cheap localdb re-snapshot happens each pass. See
//! `docs/UPDATE_LOOP.md` for the full design.

use super::dispatch;
use crate::build::{
    self, BuildFailure, ConfirmGate, InstallOpts, RunReport, Target, UpgradeSession,
};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::mirror;
use crate::names::{PkgBase, PkgName};
use crate::pacman::alpm_db::{self, PacmanIndex};
use crate::pacman::invoke::{PkgUpgrade, REPO_AUR};
use crate::resolver::Plan;
use crate::ui::{self, RowAnnotations, RowStatus, UpgradeSelection};
use std::collections::{HashMap, HashSet};
use tracing::{info, instrument};

/// Cross-batch state the loop carries for the whole session. None of it is
/// persisted to disk — restarting `gaur` starts fresh. Keyed by [`PkgBase`]
/// (the AUR build unit), matching [`RunReport`].
#[derive(Default)]
struct SessionState {
    /// PKGBUILDs the user approved this session — suppresses repeat review
    /// prompts on retry. Threaded straight into the build pipeline.
    reviewed: HashSet<PkgBase>,
    /// Builds (or their stratum's `pacman -U`) that failed, with the reason.
    failed: HashMap<PkgBase, BuildFailure>,
    /// Builds Ctrl+C'd back to the table (populated once SIGINT handling lands).
    interrupted: HashSet<PkgBase>,
    /// Skipped at review, or auto-skipped because a dep failed.
    skipped: HashSet<PkgBase>,
}

impl SessionState {
    /// Fold one batch's [`RunReport`] into the session sets. A pkgbase that
    /// succeeded this batch clears any earlier failure/skip/interrupt badge;
    /// new failures and skips are recorded for the next picker pass.
    fn fold(&mut self, report: &RunReport) {
        for pb in &report.installed {
            self.failed.remove(pb);
            self.skipped.remove(pb);
            self.interrupted.remove(pb);
        }
        self.failed
            .extend(report.failed.iter().map(|(pb, f)| (pb.clone(), f.clone())));
        self.interrupted.extend(report.interrupted.iter().cloned());
        self.skipped.extend(report.skipped_user.iter().cloned());
        // A dep-blocked pkgbase didn't build; treat it like a skip so it shows
        // dim and unchecked but stays available for a retry once its blocker is
        // dealt with.
        self.skipped.extend(report.skipped_dep.keys().cloned());
    }
}

/// One batch's outcome, as the loop's control flow sees it.
enum BatchOutcome {
    /// User declined at the change-set confirm — re-enter the table.
    Declined,
    /// Applied; fold the per-pkgbase report into session state.
    Applied(RunReport),
}

/// The loop's interactions with the outside world, behind a trait so the
/// control flow ([`drive`]) is unit-testable with a scripted fake. The real
/// implementation ([`RealEnv`]) wires the mirror snapshot, picker, and build
/// pipeline; the I/O details it owns are covered by the podman e2e test.
trait LoopEnv {
    /// Re-snapshot the localdb and recompute the remaining candidates.
    fn recompute(&self) -> Result<Vec<PkgUpgrade>>;
    /// The pkgbase owning a foreign pkgname (for keying session badges).
    fn pkgbase_of(&self, name: &PkgName) -> Option<PkgBase>;
    /// Show the picker (with badges overlaid) and return the user's selection.
    fn pick(
        &self,
        candidates: &[PkgUpgrade],
        annotations: &RowAnnotations,
    ) -> Result<UpgradeSelection>;
    /// Preview + confirm + apply one selected batch.
    fn run_batch(
        &mut self,
        candidates: &[PkgUpgrade],
        sel: &UpgradeSelection,
        reviewed: &mut HashSet<PkgBase>,
    ) -> Result<BatchOutcome>;
}

/// Run the interactive upgrade loop for the no-arg `gaur` invocation.
///
/// Refreshes the databases once, loads the session, then hands control to
/// [`drive`].
#[instrument(skip(cfg))]
pub(crate) fn run(cfg: &Config, devel: bool) -> Result<u8> {
    // Once per session: fetch the AUR mirror (+ official repo DBs) and load the
    // index. Never repeated inside the loop — picking up brand-new upstream
    // versions is what restarting `gaur` is for.
    mirror::cmd_refresh(cfg, false)?;
    let Some(session) = UpgradeSession::load(cfg)? else {
        ui::info("no AUR index; nothing to do");
        return Ok(0);
    };
    let mut env = RealEnv {
        cfg,
        session: &session,
        devel,
    };
    drive(&mut env)
}

/// The loop's pure control flow: recompute → badge → pick → apply, exiting on
/// an empty candidate list or an empty selection. Generic over [`LoopEnv`] so
/// the logic (exit conditions, report folding, badge wiring, declined re-loop)
/// is testable without a mirror, picker, or build.
fn drive<E: LoopEnv>(env: &mut E) -> Result<u8> {
    let mut state = SessionState::default();
    let mut applied_any = false;

    loop {
        let candidates = env.recompute()?;
        if candidates.is_empty() {
            ui::info(if applied_any {
                "all selected upgrades applied"
            } else {
                "nothing to do"
            });
            return Ok(0);
        }

        let annotations = annotate(env, &candidates, &state);
        let sel = env.pick(&candidates, &annotations)?;
        // Empty selection on the table is the loop's "done" signal — the user
        // looked at the remaining candidates and chose to stop here.
        if sel.is_empty() {
            return Ok(0);
        }

        // A declined batch falls through and re-enters the table; an applied
        // one folds into session state.
        if let BatchOutcome::Applied(report) =
            env.run_batch(&candidates, &sel, &mut state.reviewed)?
        {
            state.fold(&report);
            applied_any = true;
        }
    }
}

/// Overlay the session's failed/interrupted/skipped/reviewed history onto the
/// current candidates' AUR rows, keyed by pkgname for the picker.
fn annotate<E: LoopEnv>(
    env: &E,
    candidates: &[PkgUpgrade],
    state: &SessionState,
) -> RowAnnotations {
    let mut ann = RowAnnotations::default();
    for u in candidates {
        if u.repo != REPO_AUR {
            continue;
        }
        let Some(pb) = env.pkgbase_of(&u.name) else {
            continue;
        };
        if state.failed.contains_key(&pb) {
            ann.set_status(u.name.clone(), RowStatus::Failed);
        } else if state.interrupted.contains(&pb) {
            ann.set_status(u.name.clone(), RowStatus::Interrupted);
        } else if state.skipped.contains(&pb) {
            ann.set_status(u.name.clone(), RowStatus::Skipped);
        }
        if state.reviewed.contains(&pb) {
            ann.mark_reviewed(u.name.clone());
        }
    }
    ann
}

/// The production [`LoopEnv`]: a refreshed session + the live picker and build
/// pipeline.
struct RealEnv<'a> {
    cfg: &'a Config,
    session: &'a UpgradeSession,
    devel: bool,
}

impl LoopEnv for RealEnv<'_> {
    fn recompute(&self) -> Result<Vec<PkgUpgrade>> {
        self.session.recompute_remaining(self.devel)
    }

    fn pkgbase_of(&self, name: &PkgName) -> Option<PkgBase> {
        self.session.pkgbase_of(name).cloned()
    }

    fn pick(
        &self,
        candidates: &[PkgUpgrade],
        annotations: &RowAnnotations,
    ) -> Result<UpgradeSelection> {
        ui::select_upgrades(candidates, self.cfg, false, annotations)
            .map_err(|e| Error::other(format!("upgrade selection: {e}")))
    }

    fn run_batch(
        &mut self,
        candidates: &[PkgUpgrade],
        sel: &UpgradeSelection,
        reviewed: &mut HashSet<PkgBase>,
    ) -> Result<BatchOutcome> {
        // Resolve the AUR half once (if any): the plan feeds both the
        // change-set preview and the apply, so the split-package prompt inside
        // the resolver doesn't fire twice. The system-db snapshot also backs
        // both — `pacman` acts against that db.
        let pac = system_pac()?;
        let resolved = resolve_aur(self.cfg, self.session, &pac, sel)?;

        // Whole-batch preview (repo roots + AUR roots + pulled-in deps), then a
        // single gate before any sudo or build. Sizes come from the freshly
        // refreshed *synced* db — the same source as the candidate versions —
        // not the system db: gitaur never `-Sy`s the system db, so its syncdb
        // still holds the installed versions, whose cached archives make
        // `download_size()` report a misleading `0 B`.
        let size_pac = synced_pac()?;
        preview(candidates, sel, resolved.as_ref(), &size_pac);
        if !ui::confirm("Proceed with this batch?", false)
            .map_err(|e| Error::other(format!("confirm: {e}")))?
        {
            return Ok(BatchOutcome::Declined);
        }

        // Repo upgrades go through pacman -Syu (with --ignore for deselected
        // rows); AUR upgrades go through the build pipeline against the
        // already-loaded index. Repo first so AUR builds see the upgraded libs.
        dispatch::run_repo_upgrade(self.cfg, sel)?;
        let report = if let Some(plan) = resolved {
            let opts = InstallOpts {
                noconfirm: false,
                asdeps: false,
                gate: ConfirmGate::AlreadyConfirmed,
            };
            let report =
                build::apply_plan(self.cfg, self.session.index(), &pac, &plan, opts, reviewed)?;
            info!(
                installed = report.installed.len(),
                failed = report.failed.len(),
                "aur batch complete"
            );
            report
        } else {
            RunReport::default()
        };
        Ok(BatchOutcome::Applied(report))
    }
}

/// A system-dbpath pacman snapshot. Not the rootless synced db: `pacman -S`/`-U`
/// resolve against the system db, so the install plan must match what pacman
/// would actually act on.
fn system_pac() -> Result<PacmanIndex> {
    let alpm = alpm_db::open()?;
    Ok(PacmanIndex::build(&alpm))
}

/// A rootless-synced pacman snapshot, used only for the change-set preview's
/// size figures. The candidate versions came from this freshly-refreshed db, so
/// its syncdb carries the *new* versions whose archives aren't in the package
/// cache — `download_size()` then reports the real fetch cost rather than the
/// `0 B` the stale system syncdb yields for already-cached installed versions.
/// Localdb (the AUR `isize` source) is shared with [`system_pac`] via the
/// private dbpath's `local` symlink, so AUR estimates match either way.
fn synced_pac() -> Result<PacmanIndex> {
    let alpm = alpm_db::open_synced()?;
    Ok(PacmanIndex::build(&alpm))
}

/// Resolve the selected AUR upgrades into a [`Plan`], or `None` when nothing AUR
/// was selected. `PkgUpgrade.name` is the foreign pkgname the picker matched;
/// it rides along as the counterpart hint so review labelling lands on the right
/// installed pkg.
fn resolve_aur(
    cfg: &Config,
    session: &UpgradeSession,
    pac: &PacmanIndex,
    sel: &UpgradeSelection,
) -> Result<Option<Plan>> {
    if sel.aur.is_empty() {
        return Ok(None);
    }
    let targets: Vec<Target> = sel
        .aur
        .iter()
        .map(|p| Target::with_hint(p.name.clone().into_inner(), p.name.clone()))
        .collect();
    let plan = build::resolve_targets(
        cfg,
        session.index(),
        Some(session.secondary()),
        pac,
        &targets,
        false,
    )?;
    Ok(Some(plan))
}

/// Render the change-set preview: the selected root upgrades (repo + AUR) plus
/// the dependencies the AUR builds pull in, with per-row and total sizes read
/// from `pac`.
fn preview(
    candidates: &[PkgUpgrade],
    sel: &UpgradeSelection,
    resolved: Option<&Plan>,
    pac: &PacmanIndex,
) {
    // Roots: repo upgrades the user selected (look up their versions in the
    // candidate list) plus the AUR upgrades, which already carry versions.
    let mut roots: Vec<PkgUpgrade> = Vec::new();
    for name in &sel.repo {
        if let Some(u) = candidates.iter().find(|u| u.name == *name) {
            roots.push(u.clone());
        }
    }
    roots.extend(sel.aur.iter().cloned());

    let (repo_deps, aur_deps): (Vec<PkgName>, Vec<PkgBase>) = match resolved {
        None => (Vec::new(), Vec::new()),
        Some(plan) => {
            // `transitive_repo` holds the concrete pkgnames the resolver chose;
            // re-narrow them for display.
            let repo_deps = plan.transitive_repo.iter().map(PkgName::from).collect();
            // Every AUR pkgbase that's built but wasn't a selected root.
            let roots_set: HashSet<&PkgBase> = plan.direct_aur.iter().collect();
            let aur_deps = plan
                .aur_strata
                .iter()
                .flatten()
                .filter(|pb| !roots_set.contains(pb))
                .cloned()
                .collect();
            (repo_deps, aur_deps)
        }
    };
    ui::change_set_table(&roots, &repo_deps, &aur_deps, pac);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::Version;
    use std::cell::RefCell;

    fn pb(s: &str) -> PkgBase {
        PkgBase::from(s)
    }

    fn aur_row(name: &str) -> PkgUpgrade {
        PkgUpgrade {
            repo: REPO_AUR.into(),
            name: name.into(),
            old_ver: Version::from("1-1"),
            new_ver: Version::from("2-1"),
        }
    }

    fn select(names: &[&str]) -> UpgradeSelection {
        UpgradeSelection {
            repo: Vec::new(),
            repo_skipped: Vec::new(),
            aur: names.iter().map(|n| aur_row(n)).collect(),
        }
    }

    fn failed_report(name: &str) -> RunReport {
        let mut r = RunReport::default();
        r.failed
            .insert(pb(name), BuildFailure::Build("boom".into()));
        r
    }

    /// Per-pick `(pkgname, status)` snapshot of the annotations the loop fed
    /// the picker.
    type PickSnapshot = Vec<(PkgName, Option<RowStatus>)>;

    /// Scripted [`LoopEnv`]: each call pops the next queued result, and `pick`
    /// snapshots the badge state it was handed so tests can assert the loop fed
    /// the picker the right annotations. `pkgbase_of` is identity (test pkgs are
    /// non-split, so pkgname == pkgbase lexically).
    #[derive(Default)]
    struct FakeEnv {
        recompute: RefCell<Vec<Vec<PkgUpgrade>>>,
        picks: RefCell<Vec<UpgradeSelection>>,
        batches: RefCell<Vec<BatchOutcome>>,
        seen: RefCell<Vec<PickSnapshot>>,
        batch_calls: RefCell<usize>,
    }

    impl LoopEnv for FakeEnv {
        fn recompute(&self) -> Result<Vec<PkgUpgrade>> {
            Ok(self.recompute.borrow_mut().remove(0))
        }

        fn pkgbase_of(&self, name: &PkgName) -> Option<PkgBase> {
            Some(PkgBase::from(name.clone().into_inner()))
        }

        fn pick(
            &self,
            candidates: &[PkgUpgrade],
            annotations: &RowAnnotations,
        ) -> Result<UpgradeSelection> {
            let snap = candidates
                .iter()
                .map(|u| (u.name.clone(), annotations.status_of(&u.name)))
                .collect();
            self.seen.borrow_mut().push(snap);
            Ok(self.picks.borrow_mut().remove(0))
        }

        fn run_batch(
            &mut self,
            _candidates: &[PkgUpgrade],
            _sel: &UpgradeSelection,
            _reviewed: &mut HashSet<PkgBase>,
        ) -> Result<BatchOutcome> {
            *self.batch_calls.borrow_mut() += 1;
            Ok(self.batches.borrow_mut().remove(0))
        }
    }

    /// Nothing to upgrade on the first pass → exit cleanly, picker never shown.
    #[test]
    fn exits_on_empty_candidates() {
        let mut env = FakeEnv {
            recompute: RefCell::new(vec![vec![]]),
            ..FakeEnv::default()
        };
        assert_eq!(drive(&mut env).unwrap(), 0);
        assert_eq!(*env.batch_calls.borrow(), 0);
        assert!(env.seen.borrow().is_empty(), "picker should not be shown");
    }

    /// Candidates exist but the user selects nothing → "done", no batch run.
    #[test]
    fn exits_on_empty_selection() {
        let mut env = FakeEnv {
            recompute: RefCell::new(vec![vec![aur_row("a")]]),
            picks: RefCell::new(vec![UpgradeSelection::default()]),
            ..FakeEnv::default()
        };
        assert_eq!(drive(&mut env).unwrap(), 0);
        assert_eq!(*env.batch_calls.borrow(), 0);
        assert_eq!(env.seen.borrow().len(), 1);
    }

    /// Apply a batch, then loop again; the now-empty recompute ends it. The
    /// batch runs exactly once.
    #[test]
    fn applies_then_loops_until_empty() {
        let mut report = RunReport::default();
        report.installed.push(pb("a"));
        let mut env = FakeEnv {
            recompute: RefCell::new(vec![vec![aur_row("a")], vec![]]),
            picks: RefCell::new(vec![select(&["a"])]),
            batches: RefCell::new(vec![BatchOutcome::Applied(report)]),
            ..FakeEnv::default()
        };
        assert_eq!(drive(&mut env).unwrap(), 0);
        assert_eq!(*env.batch_calls.borrow(), 1);
        // recompute drained: first pass + the post-apply pass.
        assert!(env.recompute.borrow().is_empty());
    }

    /// A failed build folds into session state and badges the row on the next
    /// pass — the core fold→annotate wiring, end to end through `drive`.
    #[test]
    fn failure_badges_the_row_next_pass() {
        let mut env = FakeEnv {
            recompute: RefCell::new(vec![vec![aur_row("a")], vec![aur_row("a")], vec![]]),
            picks: RefCell::new(vec![select(&["a"]), UpgradeSelection::default()]),
            batches: RefCell::new(vec![BatchOutcome::Applied(failed_report("a"))]),
            ..FakeEnv::default()
        };
        assert_eq!(drive(&mut env).unwrap(), 0);
        let seen = env.seen.borrow();
        assert_eq!(
            seen[0][0],
            (PkgName::from("a"), None),
            "first pass: no badge"
        );
        assert_eq!(
            seen[1][0],
            (PkgName::from("a"), Some(RowStatus::Failed)),
            "second pass: failed badge after fold"
        );
    }

    /// A declined batch folds nothing, so the row is not badged on the re-loop.
    #[test]
    fn declined_batch_does_not_fold() {
        let mut env = FakeEnv {
            recompute: RefCell::new(vec![vec![aur_row("a")], vec![aur_row("a")], vec![]]),
            picks: RefCell::new(vec![select(&["a"]), UpgradeSelection::default()]),
            batches: RefCell::new(vec![BatchOutcome::Declined]),
            ..FakeEnv::default()
        };
        assert_eq!(drive(&mut env).unwrap(), 0);
        assert_eq!(*env.batch_calls.borrow(), 1);
        let seen = env.seen.borrow();
        assert_eq!(
            seen[1][0],
            (PkgName::from("a"), None),
            "declined batch must not badge the row"
        );
    }

    /// A batch's failures and skips (user-skip and dep-block) accumulate into
    /// the session sets so the next picker pass can badge them.
    #[test]
    fn fold_records_failures_and_skips() {
        let mut report = RunReport::default();
        report
            .failed
            .insert(pb("a"), BuildFailure::Build("boom".into()));
        report.skipped_user.push(pb("b"));
        report.skipped_dep.insert(pb("c"), pb("a"));
        report.interrupted.push(pb("d"));

        let mut state = SessionState::default();
        state.fold(&report);

        assert!(state.failed.contains_key(&pb("a")));
        assert!(state.skipped.contains(&pb("b")));
        assert!(
            state.skipped.contains(&pb("c")),
            "dep-blocked pkgbase should fold into skipped"
        );
        assert!(state.interrupted.contains(&pb("d")));
    }

    /// A pkgbase that succeeds this batch sheds any badge it carried from an
    /// earlier failed/skipped attempt — the retry worked, so the row should
    /// drop out cleanly rather than reappear badged.
    #[test]
    fn fold_install_clears_prior_badges() {
        let mut state = SessionState::default();
        state
            .failed
            .insert(pb("a"), BuildFailure::Build("old".into()));
        state.skipped.insert(pb("a"));
        state.interrupted.insert(pb("a"));

        let mut report = RunReport::default();
        report.installed.push(pb("a"));
        state.fold(&report);

        assert!(!state.failed.contains_key(&pb("a")));
        assert!(!state.skipped.contains(&pb("a")));
        assert!(!state.interrupted.contains(&pb("a")));
    }
}
