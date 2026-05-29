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
use crate::ui::{self, RowAnnotations, RowStatus};
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

/// Run the interactive upgrade loop for the no-arg `gaur` invocation.
///
/// Refreshes the databases once, then iterates: snapshot the localdb,
/// recompute the remaining repo+AUR upgrade candidates, show the picker, and
/// apply the chosen batch. An empty candidate list or an empty selection ends
/// the loop with a clean `Ok(0)` — the only ways out other than a hard error.
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

    let mut state = SessionState::default();
    let mut applied_any = false;

    loop {
        let candidates = session.recompute_remaining(devel)?;
        if candidates.is_empty() {
            ui::info(if applied_any {
                "all selected upgrades applied"
            } else {
                "nothing to do"
            });
            return Ok(0);
        }

        let annotations = annotate(&session, &candidates, &state);
        let sel = ui::select_upgrades(&candidates, cfg, false, &annotations)
            .map_err(|e| Error::other(format!("upgrade selection: {e}")))?;
        // Empty selection on the table is the loop's "done" signal — the user
        // looked at the remaining candidates and chose to stop here.
        if sel.is_empty() {
            return Ok(0);
        }

        // Resolve the AUR half once (if any): the plan feeds both the
        // change-set preview and the apply, so the split-package prompt inside
        // the resolver doesn't fire twice. The system-db snapshot also backs
        // both — `pacman` acts against that db.
        let pac = system_pac()?;
        let resolved = resolve_aur(cfg, &session, &pac, &sel)?;

        // Whole-batch preview (repo roots + AUR roots + pulled-in deps), then a
        // single gate before any sudo or build.
        preview(&candidates, &sel, resolved.as_ref());
        if !ui::confirm("Proceed with this batch?", false)
            .map_err(|e| Error::other(format!("confirm: {e}")))?
        {
            // Declined — back to the table; the user can re-pick or exit.
            continue;
        }

        // Repo upgrades go through pacman -Syu (with --ignore for deselected
        // rows); AUR upgrades go through the build pipeline against the
        // already-loaded index. Repo first so AUR builds see the upgraded libs.
        dispatch::run_repo_upgrade(cfg, &sel)?;
        if let Some(plan) = resolved {
            let opts = InstallOpts {
                noconfirm: false,
                asdeps: false,
                gate: ConfirmGate::AlreadyConfirmed,
            };
            let report =
                build::apply_plan(cfg, session.index(), &pac, &plan, opts, &mut state.reviewed)?;
            info!(
                installed = report.installed.len(),
                failed = report.failed.len(),
                "aur batch complete"
            );
            state.fold(&report);
        }
        applied_any = true;
    }
}

/// Overlay the session's failed/interrupted/skipped/reviewed history onto the
/// current candidates' AUR rows, keyed by pkgname for the picker.
fn annotate(
    session: &UpgradeSession,
    candidates: &[PkgUpgrade],
    state: &SessionState,
) -> RowAnnotations {
    let mut ann = RowAnnotations::default();
    for u in candidates {
        if u.repo != REPO_AUR {
            continue;
        }
        let Some(pb) = session.pkgbase_of(&u.name) else {
            continue;
        };
        if state.failed.contains_key(pb) {
            ann.set_status(u.name.clone(), RowStatus::Failed);
        } else if state.interrupted.contains(pb) {
            ann.set_status(u.name.clone(), RowStatus::Interrupted);
        } else if state.skipped.contains(pb) {
            ann.set_status(u.name.clone(), RowStatus::Skipped);
        }
        if state.reviewed.contains(pb) {
            ann.mark_reviewed(u.name.clone());
        }
    }
    ann
}

/// A system-dbpath pacman snapshot. Not the rootless synced db: `pacman -S`/`-U`
/// resolve against the system db, so the install plan must match what pacman
/// would actually act on.
fn system_pac() -> Result<PacmanIndex> {
    let alpm = alpm_db::open()?;
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
    sel: &ui::UpgradeSelection,
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
/// the dependencies the AUR builds pull in.
fn preview(candidates: &[PkgUpgrade], sel: &ui::UpgradeSelection, resolved: Option<&Plan>) {
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
    ui::change_set_table(&roots, &repo_deps, &aur_deps);
}

#[cfg(test)]
mod tests {
    use super::{BuildFailure, PkgBase, RunReport, SessionState};

    fn pb(s: &str) -> PkgBase {
        PkgBase::from(s)
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
