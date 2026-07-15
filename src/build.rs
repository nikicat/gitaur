//! Build orchestration: plan → batched repo deps → unprivileged build loop → final batched install.
//!
//! Sudo is deferred to the very end and prompted exactly once for the `pacman -U`
//! step. Builds are idempotent on the artifact: a pkgbase whose worktree
//! already holds a `.pkg.tar.{zst,xz}` named at the AUR index's exact
//! `[epoch:]pkgver-pkgrel` for every required pkgname is skipped, so
//! re-running after declining the install just replays the install step.
//! VCS pkgbases never hit this cache (their static pkgver is overridden by
//! `pkgver()`), which is the right thing — they're rebuilt on demand.

use crate::config::Config;
use crate::context;
use crate::error::{Error, Result};
use crate::index::{self, AurIndexData};
use crate::mirror::{self, MirrorRepo, RefreshOutcome, RefreshReason};
use crate::names::{PkgBase, PkgName, PkgTarget, PkgTargetSetExt};
use crate::pacman::alpm_db::{self, PacmanIndex};
use crate::pacman::invoke;
use crate::paths;
use crate::resolver::{self, PkgbasePlan, Plan};
use crate::ui;
use crate::version::Version;
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Instant;
use tracing::{debug, error, info, instrument, warn};

pub mod install;
pub mod makepkg;
pub mod metrics;
pub mod print;
pub mod review;
pub mod upgrade;

pub use upgrade::{DevelPolicy, cmd_query_upgrades, collect_upgrade_plan};

/// Read-only mirror of [`prepare_one`]'s idempotency check.
///
/// For the upgrade picker and change-set preview: whether every pkgname in
/// `pkgnames` already has a built `.pkg.tar.{zst,xz}` at `version` in
/// `pkgbase`'s worktree, so a `pacman -U` would reuse the artifact instead of
/// rebuilding. The caller chooses the scope: a **single** pkgname for one
/// split-package row (the picker lists each pkgname separately, so each gets
/// its own `built` tag), or the pkgbase's **full** pkgname set to ask "is this
/// whole pkgbase's build done?".
///
/// Touches only the on-disk worktree — no fetch, no `makepkg`, no localdb — so
/// it's safe to call once per candidate while drawing the picker. VCS pkgbases
/// (dynamic `pkgver`) never match here, which is correct: they always rebuild.
/// An empty `pkgnames` is never "built" (matches the cache check, which would
/// otherwise vacuously succeed on a pkgbase that produces nothing).
pub fn artifacts_built(pkgbase: &PkgBase, version: &Version, pkgnames: &[PkgName]) -> bool {
    let Ok(found) = install::find_produced(&paths::pkg_worktree(pkgbase)) else {
        return false;
    };
    !pkgnames.is_empty()
        && pkgnames
            .iter()
            .all(|name| found.iter().any(|f| install::matches_pkg(f, name, version)))
}

/// One built pkgbase's set of `.pkg.tar.zst` outputs.
struct BuiltPkg {
    pkgbase: PkgBase,
    files: Vec<PathBuf>,
}

/// One install target paired with the user's intent for counterpart resolution.
///
/// `spec` is the freeform user-typed string (pkgname / pkgbase / virtual /
/// `name>=ver`); `hint` is the pkgname the user thinks they have installed —
/// the one [`PacmanIndex::counterpart_with_hint`] should bias the lookup
/// toward. Two callers populate it:
///
///   * **`-S <name>`**: `hint = None`. [`resolver::expand_pkgbase_targets`]
///     fills the hint in when it rewrites (e.g. `-S dotnet-runtime-7.0`
///     rewrites to pkgbase `dotnet-core-7.0-bin` with `hint =
///     PkgName("dotnet-runtime-7.0")`) — the rewritten spec lost the user's
///     original wording, so the hint preserves it.
///   * **`-Syu`**: `hint = Some(name)`. The picker hands us the foreign
///     pkgname that's already installed; that *is* the counterpart, and
///     anchoring the hint here keeps the picker's intent across the round-
///     trip through `cmd_install` without depending on heuristics.
///
/// Hints are keyed by pkgbase in [`Plan::counterpart_hints`] after expansion,
/// so `prepare_one` can look one up regardless of which input path produced it.
#[derive(Debug, Clone)]
pub struct Target {
    pub spec: String,
    pub hint: Option<PkgName>,
}

impl Target {
    /// Construct a target with no explicit hint — the resolver may infer one
    /// from `spec` when rewriting.
    pub fn bare(spec: impl Into<String>) -> Self {
        Self {
            spec: spec.into(),
            hint: None,
        }
    }

    /// Construct a target with an explicit hint — used by `-Syu` where the
    /// picker already knows which installed pkgname triggered the upgrade.
    pub fn with_hint(spec: impl Into<String>, hint: PkgName) -> Self {
        Self {
            spec: spec.into(),
            hint: Some(hint),
        }
    }
}

/// Whether the plan-level "Proceed with installation?" prompt still needs to be
/// asked.
///
/// [`ConfirmGate::AlreadyConfirmed`] means a higher layer already gated the
/// batch (the `-Syu` picker, the no-arg loop's change-set preview), so
/// `install_with_index` skips its own prompt. The sudo gate and PKGBUILD review
/// prompts fire regardless.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmGate {
    Ask,
    AlreadyConfirmed,
}

/// Knobs for one install batch, shared by the `-S` entry and the upgrade loop.
///
/// `noconfirm` / `asdeps` are the crate-wide pacman-parity flags; `gate`
/// controls only the plan-level confirm (see [`ConfirmGate`]).
#[derive(Debug, Clone, Copy)]
pub struct InstallOpts {
    pub noconfirm: bool,
    pub asdeps: bool,
    pub gate: ConfirmGate,
}

/// Whether the unknown-target path may offer the one-time AUR setup.
///
/// The caller answers it from what already happened this invocation:
/// `-Sy <targets>` runs the sync consent prompt before the install half
/// ([`offer_aur_setup`]), and a user who just declined it must not be asked
/// the same question again seconds later (`mirror/consent.rs`: no
/// double-prompts after an informed explicit command).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupOffer {
    /// Nothing asked yet this invocation — the offer may prompt (still
    /// subject to [`offer_applies`]).
    Open,
    /// An explicit sync in this same invocation already asked and was
    /// declined; the answer stands for the rest of the invocation.
    AlreadyDeclined,
}

/// Entry point for `aurox -S <targets>`.
///
/// Loads the pacman snapshot and (optionally) the AUR index in parallel, then
/// hands them to [`install_with_index`]. After printing the unified plan and
/// getting a single confirmation aurox drives every `pacman` call with
/// `--noconfirm` so the user is asked once; pacman never re-prompts.
#[instrument(skip(cfg))]
pub fn cmd_install(
    cfg: &Config,
    targets: &[Target],
    opts: InstallOpts,
    offer: SetupOffer,
) -> Result<u8> {
    // Probed once: drives only the unknown-target wording below. The data
    // flow is uniform — an unavailable AUR loads as empty AUR data.
    let aur_state = index::AurState::probe(cfg);

    // Pacman snapshot + AUR index loaded concurrently. PacmanIndex iterates
    // every sync DB and the localdb (tens of ms on a typical system); the
    // AUR mmap + rkyv deserialize is similar. `context::join` hides one behind
    // the other and propagates the caller's context so `load_or_resync` sees
    // `--noresync` and the right `state_dir()` even on the stolen worker.
    let (pac_res, aur_res) = context::join(
        || -> Result<PacmanIndex> {
            let alpm = alpm_db::open()?;
            Ok(PacmanIndex::build(&alpm))
        },
        || AurIndexData::load(cfg),
    );
    let pac = pac_res?;
    let aur_data = aur_res?;

    // `-S` has no cross-call review memory, so a fresh per-invocation set. The
    // upgrade loop keeps its own session-scoped set across batches instead.
    let mut reviewed = HashSet::new();
    let ctx = InstallCtx {
        cfg,
        aur: &aur_data,
        pac: &pac,
    };
    let report = match ctx.install(targets, opts, &mut reviewed) {
        // Unresolvable targets: when no AUR data was in play the names may
        // simply live in the (unavailable) AUR — offer to bring it in and
        // retry once, else say what would.
        Err(Error::UnknownTargets(names)) => {
            let Some(aur_data) = offer_aur_setup(cfg, &names, aur_state, opts.noconfirm, offer)?
            else {
                return Err(Error::UnknownTargets(unknown_targets_hint(
                    names, aur_state,
                )));
            };
            let ctx = InstallCtx {
                cfg,
                aur: &aur_data,
                pac: &pac,
            };
            match ctx.install(targets, opts, &mut reviewed) {
                // Still unknown with a fresh index: genuinely not a package.
                Err(Error::UnknownTargets(names)) => {
                    return Err(Error::UnknownTargets(unknown_targets_hint(
                        names,
                        index::AurState::probe(cfg),
                    )));
                }
                other => other?,
            }
        }
        other => other?,
    };
    // A Ctrl+C'd build exits non-zero too: the user aborted, so `||` chains and
    // scripts should see the run didn't complete.
    Ok(u8::from(
        report.had_failures() || !report.interrupted.is_empty(),
    ))
}

/// After `-S` hit unknown targets with the AUR enabled-but-unsynced: the
/// names may simply live in the AUR, so offer the one-time mirror setup —
/// through the consent gate's [`RefreshReason::InstallOffer`] row, which
/// announces the cost and prompts — and return the freshly loaded index on
/// "yes". `None` ⇒ the offer doesn't apply ([`offer_applies`]) or the user
/// declined; the caller reports the plain unknown-target error instead.
fn offer_aur_setup(
    cfg: &Config,
    names: &str,
    aur: index::AurState,
    noconfirm: bool,
    offer: SetupOffer,
) -> Result<Option<AurIndexData>> {
    if !offer_applies(aur, noconfirm, std::io::stdin().is_terminal(), offer) {
        return Ok(None);
    }
    ui::note(&format!(
        "{names}: not in the official repos — may be in the AUR"
    ));
    match mirror::cmd_refresh(cfg, RefreshReason::InstallOffer)? {
        RefreshOutcome::Refreshed => Ok(Some(AurIndexData::load(cfg)?)),
        RefreshOutcome::AurSkipped(_) => Ok(None),
    }
}

/// Whether an unknown-target failure should offer the AUR setup: only when
/// the AUR is enabled-but-unsynced (with a ready index the names are
/// genuinely unknown; pacman-only mode is a standing choice), only where
/// a human can answer — a TTY without `--noconfirm` — and never when this
/// invocation's explicit sync already asked and was declined
/// ([`SetupOffer::AlreadyDeclined`]). The consent gate enforces the
/// TTY/`--noconfirm` rule too ([`RefreshReason::InstallOffer`]); this
/// pre-gate just avoids a pointless repo-DB refresh on the refusal path.
const fn offer_applies(
    aur: index::AurState,
    noconfirm: bool,
    stdin_is_tty: bool,
    offer: SetupOffer,
) -> bool {
    matches!(offer, SetupOffer::Open)
        && matches!(aur, index::AurState::NotSetUp)
        && !noconfirm
        && stdin_is_tty
}

/// Suffix an [`Error::UnknownTargets`] with why the AUR couldn't answer: when
/// no AUR data was in play the unresolved names may simply be AUR packages,
/// so point at the missing half — how to enable it, or (in pacman-only mode)
/// why it's off. With a ready index the names are genuinely unknown and pass
/// through unchanged.
fn unknown_targets_hint(names: String, aur: index::AurState) -> String {
    match aur {
        index::AurState::Ready => names,
        index::AurState::NotSetUp => {
            format!("{names} (no AUR index — run `aurox -Sy` to enable AUR installs)")
        }
        index::AurState::Disabled => {
            format!("{names} (AUR disabled: aur = false in config.toml)")
        }
    }
}

/// Everything one resolve/install pass *reads*: config plus the two package
/// sources. Groups the `(cfg, idx, pac)` caravan the resolve/build call
/// chain used to thread through every signature; the mutable run state lives
/// in [`PipelineRun`].
pub(crate) struct InstallCtx<'a> {
    pub cfg: &'a Config,
    pub aur: &'a AurIndexData,
    pub pac: &'a PacmanIndex,
}

impl InstallCtx<'_> {
    /// Resolve `targets` against this context, then drive the repo and AUR
    /// install phases, returning the per-pkgbase [`RunReport`].
    ///
    /// Split out of [`cmd_install`] so the no-arg upgrade loop can run many
    /// batches against one already-loaded context without re-reading the index
    /// each iteration, and so the loop can fold the report into its session
    /// state. `reviewed` is the session-scoped set of pkgbases already approved
    /// (see [`Self::prepare_one`]); `-S` passes a fresh empty one.
    pub(crate) fn install(
        &self,
        targets: &[Target],
        opts: InstallOpts,
        reviewed: &mut HashSet<PkgBase>,
    ) -> Result<RunReport> {
        let plan = self.resolve_targets(targets, opts.noconfirm)?;
        print::plan(&plan, self.aur.index(), self.pac);
        self.apply_plan(&plan, opts, reviewed)
    }

    /// Classify `targets` into a [`Plan`] — expand bare pkgbases, resolve deps, and
    /// stamp the per-pkgbase decisions (split selections, counterpart hints) onto
    /// it.
    ///
    /// Pulled out of [`Self::install`] so the upgrade loop can resolve once,
    /// render its change-set preview against the [`Plan`], and then hand the *same*
    /// plan to [`apply_plan`] — re-resolving would re-run the split-package prompt
    /// inside `expand_pkgbase_targets`.
    pub(crate) fn resolve_targets(&self, targets: &[Target], noconfirm: bool) -> Result<Plan> {
        // Expand bare `-S <pkgbase>` targets into the pkgname(s) the user wants
        // installed as explicit. Split pkgbases prompt for a subset; single-pkgname
        // pkgbases pass through silently. The selector closure delegates to
        // `ui::select_pkgnames` so tests can swap in a deterministic picker.
        let expanded =
            resolver::expand_pkgbase_targets(self.aur, self.pac, targets, &mut |pb, pns| {
                ui::select_pkgnames(pb, pns, noconfirm).map_err(|e| Error::other(e.to_string()))
            })?;
        let mut plan = resolver::resolve(self.aur, self.pac, &expanded.targets)?;
        plan.pkgname_selections = expanded.selections;
        // For pkgbase/provides hits the resolver received the pkgbase string, so
        // `plan.direct_targets` only contains the pkgbase. Mark the pkgnames the
        // user actually chose as direct too, so `install_stratum` flips their
        // `.pkg.tar.zst` to Explicit (instead of `--asdeps`). The expanded
        // pkgnames widen into `PkgTarget` — explicit "treat this pkgname as
        // an unclassified user target for the install-reason check."
        plan.direct_targets
            .extend(expanded.direct_pkgnames.into_iter().map(PkgTarget::from));
        plan.counterpart_hints = expanded.counterpart_hints;
        Ok(plan)
    }

    /// Execute an already-resolved [`Plan`]: confirm gate, repo phase, AUR
    /// pipeline. The caller is responsible for displaying the plan first
    /// ([`print::plan`] for `-S`, the change-set preview for the loop).
    pub(crate) fn apply_plan(
        &self,
        plan: &Plan,
        opts: InstallOpts,
        reviewed: &mut HashSet<PkgBase>,
    ) -> Result<RunReport> {
        if plan.direct_repo.is_empty()
            && plan.transitive_repo.is_empty()
            && plan.aur_strata.is_empty()
        {
            return Ok(RunReport::default());
        }

        // Skip the plan confirm when every package was explicitly named — the
        // table above just echoes the user's own request. The prompt earns its
        // place only when the plan drags in unrequested packages (repo deps or
        // AUR makedepends). The sudo "Continue?" gate (and AUR review prompts)
        // still fire regardless.
        if opts.gate == ConfirmGate::Ask
            && !plan.only_requested()
            && !ui::confirm("Proceed with installation?", opts.noconfirm)?
        {
            return Err(Error::UserAbort);
        }

        install_repo_phase(self.cfg, plan, opts.asdeps)?;

        if plan.aur_strata.is_empty() {
            return Ok(RunReport::default());
        }
        // A non-empty `aur_strata` implies the resolver found AUR entries, which
        // can only happen when real AUR data was loaded — so `self.aur` here is
        // the loaded index, never the empty fallback.
        self.run_aur_pipeline(plan, opts, reviewed)
    }

    /// Stratified AUR build+install loop with per-pkgbase failure isolation.
    ///
    /// For each stratum (set of AUR pkgbases whose build-time deps are all in
    /// earlier strata): build every pkgbase, then `pacman -U` the resulting
    /// `.pkg.tar.zst`'s so the next stratum's `makepkg` finds them in localdb.
    /// Sudo cache (typically 5-15 min) bridges per-stratum sudo prompts. Plain
    /// runtime `depends` are resolved by the final stratum's `pacman -U`
    /// resolving against the same batch. After all strata, transitive AUR pkgs
    /// that ended up Explicit during their stratum's `-U` are flipped to
    /// `--asdeps` via a single cheap `pacman -D` call.
    ///
    /// A single makepkg failure no longer aborts the run: the offending pkgbase
    /// is marked failed, anything in the closure of `plan.aur_make_edges`
    /// rooted at it is auto-skipped (its deps wouldn't be in localdb anyway),
    /// and the remaining independent pkgbases keep building. A final summary
    /// lists installed / failed / skipped pkgbases; the return code is non-zero
    /// iff anything failed or was skipped due to a dep failure.
    fn run_aur_pipeline(
        &self,
        plan: &Plan,
        opts: InstallOpts,
        reviewed: &mut HashSet<PkgBase>,
    ) -> Result<RunReport> {
        let mirror = MirrorRepo::open(&paths::aur_repo_path())?;
        let mut run = PipelineRun {
            direct: &plan.direct_targets,
            asdeps: opts.asdeps,
            transitive_marks: Vec::new(),
            report: RunReport::default(),
        };

        // Phase 1: open every worktree, run idempotency checks, and prompt the
        // user for review across all strata up front. Skipped pkgbases are
        // dropped; an "abort" propagates immediately as Error::UserAbort. No
        // makepkg runs in this phase, so the user can walk through every diff
        // before any build kicks off.
        let mut prep_strata: Vec<Vec<Prep<'_>>> = Vec::with_capacity(plan.aur_strata.len());
        // Threaded across every `prepare_one`: seeded from the run's non-interactive
        // flag, then flipped to `Auto` by a mid-pass "approve all" so the remaining
        // pkgbases skip their diff prompt.
        let mut prompting = review::Prompting::from_noconfirm(opts.noconfirm);
        for stratum in &plan.aur_strata {
            let mut row = Vec::with_capacity(stratum.len());
            for pkgbase in stratum {
                // Partial-split selection — present only when the user asked
                // for a subset. makepkg always packages every pkgname in a
                // split (no `--pkg=` flag); we filter the produced files down
                // to the selection so `install_stratum`'s `pacman -U` skips
                // the rest.
                row.push(self.prepare_one(
                    &mirror,
                    &plan.pkgbase_plan(pkgbase),
                    reviewed,
                    &mut prompting,
                )?);
            }
            prep_strata.push(row);
        }

        // Phase 2: makepkg approved pkgbases, install per-stratum so later
        // strata's makepkg finds earlier strata's deps in localdb.
        for (stratum_idx, (stratum, preps)) in plan.aur_strata.iter().zip(prep_strata).enumerate() {
            if plan.aur_strata.len() > 1 {
                ui::info(&format!(
                    "build stratum {}/{}: {}",
                    stratum_idx + 1,
                    plan.aur_strata.len(),
                    stratum.join(" "),
                ));
            }
            let built = self.build_stratum(preps, &plan.aur_make_edges, &mut run.report);
            run.commit_stratum(self, &built, stratum_idx);
            // A Ctrl+C'd build stops the whole batch here — anything already
            // built+installed above stays; the rest stays outstanding for the next
            // table pass (or a `-S` retry).
            if !run.report.interrupted.is_empty() {
                ui::warn("build interrupted — stopping this batch");
                break;
            }
        }

        let PipelineRun {
            transitive_marks,
            report,
            ..
        } = run;
        if !opts.asdeps && !transitive_marks.is_empty() {
            let mut args = vec!["-D".to_owned(), "--asdeps".into()];
            // pacman argv is `Vec<String>` — downgrade typed `PkgName`s at
            // this single boundary.
            args.extend(transitive_marks.into_iter().map(PkgName::into_inner));
            if let Err(e) = invoke::exec_pacman(self.cfg, &args) {
                // Cosmetic only: pacman will still recompute install reasons on
                // the next `-D`/`-Syu`. Warn instead of failing the run.
                ui::warn(&format!("could not flip transitive pkgs to --asdeps: {e}"));
            }
        }

        print::final_summary(&report);
        Ok(report)
    }

    /// Build every pkgbase in one stratum, mutating `report` as failures /
    /// user-skips happen. Returns the `BuiltPkg`s ready for `commit_stratum`.
    fn build_stratum(
        &self,
        preps: Vec<Prep<'_>>,
        make_edges: &HashMap<PkgBase, Vec<PkgBase>>,
        report: &mut RunReport,
    ) -> Vec<BuiltPkg> {
        let mut built: Vec<BuiltPkg> = Vec::with_capacity(preps.len());
        for prep in preps {
            // Skip anything whose makedep closure already contains a
            // failed/skipped pkgbase — the build would just fail with a
            // confusing "missing dep" error. `aur_make_edges` is the resolver's
            // pkgbase→makedep-pkgbases map, so a direct lookup is enough (the
            // transitive case was caught when the ancestor itself was skipped
            // in an earlier stratum).
            if let Some(blocker) = blocking_dep(prep.pkgbase, make_edges, report) {
                ui::warn(&format!(
                    "{}: skipping (depends on failed/skipped {blocker})",
                    prep.pkgbase,
                ));
                report
                    .skipped_dep
                    .insert(prep.pkgbase.to_owned(), blocker.to_owned());
                continue;
            }
            match prep.disposition {
                Disposition::Skipped => {
                    ui::note(&format!("{}: skipped", prep.pkgbase));
                    report.skipped_user.push(prep.pkgbase.to_owned());
                }
                Disposition::Cached(files) => built.push(BuiltPkg {
                    pkgbase: prep.pkgbase.to_owned(),
                    files,
                }),
                Disposition::Build => match run_build(self.cfg, &prep) {
                    Ok(files) => built.push(BuiltPkg {
                        pkgbase: prep.pkgbase.to_owned(),
                        files,
                    }),
                    // Ctrl+C during this build: mark it interrupted and stop the
                    // rest of the stratum. The caller installs what built so far,
                    // then bails the whole run (the no-arg loop re-enters the
                    // table; `-S` exits non-zero).
                    Err(Error::Interrupted) => {
                        ui::warn(&format!("{}: build interrupted", prep.pkgbase));
                        report.interrupted.push(prep.pkgbase.to_owned());
                        break;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        ui::error(&format!("{}: build failed: {msg}", prep.pkgbase));
                        report
                            .failed
                            .insert(prep.pkgbase.to_owned(), BuildFailure::Build(msg));
                    }
                },
            }
        }
        built
    }

    #[instrument(skip(self, mirror, target, reviewed), fields(pkgbase = %target.pkgbase))]
    fn prepare_one<'p>(
        &self,
        mirror: &MirrorRepo,
        target: &PkgbasePlan<'p>,
        reviewed: &mut HashSet<PkgBase>,
        prompting: &mut review::Prompting,
    ) -> Result<Prep<'p>> {
        let (idx, pac, history_scan_max) =
            (self.aur.index(), self.pac, self.cfg.review_history_scan_max);
        let &PkgbasePlan {
            pkgbase,
            selection,
            hint,
        } = target;
        let entry = idx
            .entries
            .iter()
            .find(|e| &e.pkgbase == pkgbase)
            .ok_or_else(|| Error::Build(format!("{pkgbase}: missing from index")))?;
        let dest = paths::pkg_worktree(pkgbase);
        let wt = mirror::worktree::add_or_reset(mirror, pkgbase, &dest)?;

        let new_ver = entry.version();
        let required: Vec<PkgName> = match selection {
            Some(sel) => sel.to_vec(),
            None => entry.pkgnames.iter().map(|p| p.name.clone()).collect(),
        };

        // Idempotency: skip rebuild iff a .pkg.tar.{zst,xz} file at exactly
        // `new_ver` already exists for every required pkgname. Derived purely
        // from on-disk artifacts — no sidecar DB needed. VCS pkgbases never hit
        // this (their static `pkgver` differs from the dynamic one makepkg
        // writes into the artifact filename), so they always rebuild, which is
        // the right behavior for `-git`/`-svn`/etc.
        let existing = install::find_produced(&wt.path)?;
        let cached = !required.is_empty()
            && required.iter().all(|name| {
                existing
                    .iter()
                    .any(|f| install::matches_pkg(f, name, &new_ver))
            });
        if cached {
            let kept = select_outputs(&existing, &required, &new_ver);
            ui::note(&format!("{pkgbase}: already built {new_ver}"));
            debug!(
                %pkgbase,
                version = %new_ver,
                files = kept.len(),
                "reusing cached build"
            );
            return Ok(Prep {
                pkgbase,
                wt,
                new_ver,
                required,
                disposition: Disposition::Cached(kept),
            });
        }

        // Already approved this session: the mirror is frozen for the whole
        // session (no mid-loop re-fetch), so a pkgbase maps to a single PKGBUILD
        // commit and re-reviewing it is pure friction. Auto-approve — this is what
        // makes the loop's retry-after-failure painless (a pkgbase approved in one
        // iteration and rebuilt in the next isn't re-prompted).
        if reviewed.contains(pkgbase) {
            debug!(%pkgbase, "PKGBUILD already reviewed this session; skipping prompt");
            return Ok(Prep {
                pkgbase,
                wt,
                new_ver,
                required,
                disposition: Disposition::Build,
            });
        }

        // What the user has installed that this pkgbase will displace. Looks
        // through pkgname → replaces → provides so renames and split pkgs label
        // correctly; see `PacmanIndex::counterpart_with_hint` for the resolution
        // order and how `hint` overrides the "first-hit" default.
        let counterpart = pac.counterpart_with_hint(entry, hint);
        // `prompting` is the pass-level state: `Auto` on a non-interactive run or
        // once "approve all" was chosen, else `Prompt`.
        let request = review::ReviewRequest {
            mirror,
            pkgbase,
            new_ver: &new_ver,
            counterpart: counterpart.as_ref(),
            wt: &wt,
            history_scan_max,
        };
        let disposition = match request.review(*prompting)? {
            review::Outcome::Approved => {
                // Remember the approval so a later batch in the same session
                // doesn't re-prompt the same diff.
                reviewed.insert(pkgbase.clone());
                Disposition::Build
            }
            review::Outcome::ApprovedAll => {
                // Approve this one and auto-approve every remaining pkgbase in the
                // pass — flip the shared state the caller threads across
                // `prepare_one` calls.
                *prompting = review::Prompting::Auto;
                reviewed.insert(pkgbase.clone());
                Disposition::Build
            }
            review::Outcome::Skipped => Disposition::Skipped,
        };
        Ok(Prep {
            pkgbase,
            wt,
            new_ver,
            required,
            disposition,
        })
    }
}

/// Install the user's repo targets up front: direct ones as explicit, deps
/// as `--asdeps`. Two `pacman -S` calls so the install-reason flag is per-
/// batch; sudo cache bridges them. No-op when both buckets are empty.
/// Always `--noconfirm`: aurox already gated this with its own prompt, so
/// pacman shouldn't ask again.
fn install_repo_phase(cfg: &Config, plan: &Plan, asdeps: bool) -> Result<()> {
    if !plan.direct_repo.is_empty() {
        ui::info("installing repo packages");
        let mut args = vec!["-S".to_owned(), "--needed".into(), "--noconfirm".into()];
        if asdeps {
            args.push("--asdeps".into());
        }
        args.extend(plan.direct_repo.iter().cloned());
        invoke::exec_pacman(cfg, &args)?;
    }
    if !plan.transitive_repo.is_empty() {
        ui::info("installing repo dependencies");
        let mut args = vec![
            "-S".to_owned(),
            "--needed".into(),
            "--noconfirm".into(),
            "--asdeps".into(),
        ];
        args.extend(plan.transitive_repo.iter().cloned());
        invoke::exec_pacman(cfg, &args)?;
    }
    Ok(())
}

/// The mutable half of one AUR pipeline run: which built pkgnames stay
/// Explicit, the `--asdeps` override, the pending install-reason flips, and
/// the accumulating per-pkgbase outcome. Groups what `commit_stratum` /
/// `install_stratum` used to take as four parallel parameters.
struct PipelineRun<'a> {
    /// The user-named targets — a built pkgname matching one of these stays
    /// Explicit (`PkgTargetSetExt::contains_pkgname` is the probe).
    direct: &'a HashSet<PkgTarget>,
    /// `--asdeps`: every built pkg installs as a dep, nothing stays Explicit.
    asdeps: bool,
    /// Pkgnames to flip to `--asdeps` once the whole run lands.
    transitive_marks: Vec<PkgName>,
    report: RunReport,
}

impl PipelineRun<'_> {
    /// Run `pacman -U` for one stratum's built pkgs and update the report with
    /// the outcome. A pacman failure is atomic, so every pkgbase in this stratum
    /// is marked failed and the next stratum's dep check skips dependents.
    fn commit_stratum(&mut self, ctx: &InstallCtx<'_>, built: &[BuiltPkg], stratum_idx: usize) {
        if built.is_empty() {
            return;
        }
        match self.install_stratum(ctx, built) {
            Ok(()) => {
                for b in built {
                    self.report.installed.push(b.pkgbase.clone());
                }
            }
            Err(e) => {
                let msg = e.to_string();
                ui::error(&format!(
                    "stratum {} install failed: {msg}",
                    stratum_idx + 1
                ));
                for b in built {
                    self.report
                        .failed
                        .insert(b.pkgbase.clone(), BuildFailure::Install(msg.clone()));
                }
            }
        }
    }

    /// Install every `.pkg.tar.zst` produced by one stratum's builds in a single
    /// `pacman -U` transaction so intra-stratum runtime deps (split packages,
    /// AUR pkg + sibling AUR dep) resolve against each other. Pkgnames that
    /// weren't on the user's command line are appended to
    /// [`Self::transitive_marks`] so the run can flip them to `--asdeps` at the
    /// very end.
    #[instrument(skip(self, ctx, built))]
    fn install_stratum(&mut self, ctx: &InstallCtx<'_>, built: &[BuiltPkg]) -> Result<()> {
        let (cfg, idx, asdeps_override) = (ctx.cfg, ctx.aur.index(), self.asdeps);
        if built.is_empty() {
            return Ok(());
        }
        let total: usize = built.iter().map(|b| b.files.len()).sum();
        ui::step(&format!("installing {total} built package(s) with pacman"));

        let mut files: Vec<PathBuf> = Vec::new();
        let mut pending_marks: Vec<PkgName> = Vec::new();
        for b in built {
            // Look up the index entry to know which pkgnames belong to this
            // pkgbase (split packages have multiple names sharing one pkgbase).
            let _entry = idx
                .entries
                .iter()
                .find(|e| e.pkgbase == b.pkgbase)
                .ok_or_else(|| Error::Build(format!("{}: missing from index", b.pkgbase)))?;
            for f in &b.files {
                files.push(f.clone());
                // makepkg artifacts always have a parseable `<name>-<ver>-<rel>-<arch>.pkg.tar.*`
                // shape — None here means a corrupted/unexpected filename slipped through, and
                // silently defaulting to an empty pkgname would misclassify the install
                // (`contains_pkgname("")` is trivially false → marked as a dep, then
                // `pacman -D --asdeps ""` is a no-op). Fail loudly instead.
                let pkgname = install::extract_pkgname(f).ok_or_else(|| {
                    Error::Build(format!(
                        "{}: cannot extract pkgname from artifact {}",
                        b.pkgbase,
                        f.display(),
                    ))
                })?;
                // `direct` is `HashSet<PkgTarget>`; `contains_pkgname` is the
                // single Borrow<str> probe site (cross-domain string match
                // between the user's typed targets and the built pkgname).
                let is_direct = !asdeps_override && self.direct.contains_pkgname(&pkgname);
                if !is_direct {
                    pending_marks.push(pkgname);
                }
            }
        }

        // Always `--noconfirm`: aurox's plan+confirm at the top of `cmd_install`
        // is the single user gate; pacman shouldn't ask again.
        let mut args = vec!["-U".to_owned(), "--needed".into(), "--noconfirm".into()];
        if asdeps_override {
            args.push("--asdeps".into());
        }
        for p in &files {
            args.push(p.to_string_lossy().into_owned());
        }
        invoke::exec_pacman(cfg, &args)?;
        // Only record install-reason flips after pacman -U succeeds — a failed
        // transaction never installed these pkgs, so a later `pacman -D --asdeps`
        // would error on them.
        self.transitive_marks.extend(pending_marks);
        Ok(())
    }
}

/// Why a pkgbase failed to land in localdb this batch.
///
/// The reason isn't free text — it's one of two pipeline phases, and the
/// distinction is actionable: a `Build` failure points the user at a makepkg
/// log to read; an `Install` failure means the package built fine but its
/// stratum's `pacman -U` transaction rejected it (and took the whole stratum
/// down with it). The inner string is the underlying [`Error`]'s rendering —
/// a log path, a pacman exit code — the only genuinely free-form part.
#[derive(Debug, Clone)]
pub(super) enum BuildFailure {
    /// `makepkg` exited non-zero or produced no packages.
    Build(String),
    /// The stratum's `pacman -U` transaction failed.
    Install(String),
}

impl BuildFailure {
    /// Short phase label for the summary line (`build` / `install`).
    pub(super) const fn phase(&self) -> &'static str {
        match self {
            Self::Build(_) => "build",
            Self::Install(_) => "install",
        }
    }

    /// The underlying error rendering.
    pub(super) fn detail(&self) -> &str {
        match self {
            Self::Build(d) | Self::Install(d) => d,
        }
    }
}

/// Per-pkgbase outcome aggregated across all strata, used to drive both the
/// dep-skip logic and the final summary (see `print::final_summary`).
///
/// Keys are typed `PkgBase` so the report can't accidentally key on a pkgname.
#[derive(Default)]
pub(super) struct RunReport {
    /// Successfully built (or reused from cache) and installed by `pacman -U`.
    pub(super) installed: Vec<PkgBase>,
    /// makepkg or the stratum's `pacman -U` returned non-zero, tagged by which
    /// phase failed so the summary can say where to look.
    pub(super) failed: HashMap<PkgBase, BuildFailure>,
    /// User chose "skip" at the PKGBUILD review prompt.
    pub(super) skipped_user: Vec<PkgBase>,
    /// Auto-skipped because a pkgbase earlier in the build graph failed.
    /// Value is the immediate blocker — usually enough to debug since the
    /// blocker itself shows up in `failed`.
    pub(super) skipped_dep: HashMap<PkgBase, PkgBase>,
    /// Ctrl+C'd mid-build. Distinct from `failed`: nothing went wrong with the
    /// package, the user just bailed — the no-arg loop badges it separately and
    /// keeps it available for a clean retry.
    pub(super) interrupted: Vec<PkgBase>,
}

impl RunReport {
    fn had_failures(&self) -> bool {
        !self.failed.is_empty() || !self.skipped_dep.is_empty()
    }

    /// No failure, dep-skip, or interrupt — every pkgbase this phase attempted
    /// landed. (A user review-skip is a choice, not a failure, so it doesn't
    /// count against the phase.)
    pub(super) fn all_landed(&self) -> bool {
        self.failed.is_empty() && self.skipped_dep.is_empty() && self.interrupted.is_empty()
    }

    /// Fold another phase's report into this one — used by the shell's apply
    /// when the run splits into a blocker-rebuild phase (installed ahead of the
    /// repo lane) and the main phase.
    pub(super) fn absorb(&mut self, other: Self) {
        self.installed.extend(other.installed);
        self.failed.extend(other.failed);
        self.skipped_user.extend(other.skipped_user);
        self.skipped_dep.extend(other.skipped_dep);
        self.interrupted.extend(other.interrupted);
    }
}

/// Return the first AUR pkgbase makedep of `pkgbase` that has already failed
/// or been skipped. `None` means `pkgbase` is safe to build.
fn blocking_dep<'a>(
    pkgbase: &PkgBase,
    make_edges: &'a HashMap<PkgBase, Vec<PkgBase>>,
    report: &RunReport,
) -> Option<&'a PkgBase> {
    let deps = make_edges.get(pkgbase)?;
    deps.iter().find(|dep| {
        report.failed.contains_key(*dep)
            || report.skipped_dep.contains_key(*dep)
            || report.skipped_user.iter().any(|s| s == *dep)
    })
}

/// One pkgbase's prepared state, produced in phase 1 and consumed in phase 2.
///
/// `required` is the concrete list of pkgnames this run must end up with
/// installed at `new_ver` — derived from `Plan.pkgname_selections` when the
/// user named a subset, otherwise widened to `entry.pkgnames`. Storing the
/// resolved list (instead of the upstream `Option`) keeps `select_outputs`
/// signature uniform and means `run_build` / the cached path filter against
/// the same source of truth as `prepare_one`'s idempotency check.
struct Prep<'a> {
    pkgbase: &'a PkgBase,
    wt: mirror::worktree::Worktree,
    new_ver: Version,
    required: Vec<PkgName>,
    disposition: Disposition,
}

/// What phase 2 should do with a [`Prep`].
enum Disposition {
    /// Already built at exactly `new_ver`; reuse the listed files.
    Cached(Vec<PathBuf>),
    /// Approved by the user (or noconfirm); run makepkg in phase 2.
    Build,
    /// User chose "skip" — drop from this run.
    Skipped,
}

#[instrument(skip(cfg, prep), fields(pkgbase = %prep.pkgbase, version = %prep.new_ver))]
fn run_build(cfg: &Config, prep: &Prep<'_>) -> Result<Vec<PathBuf>> {
    ui::step(&format!("makepkg {}", prep.pkgbase));
    let started = Instant::now();

    // VCS pkgbases resolve their real `pkgver()` only while makepkg extracts
    // sources (it then rewrites `pkgver=` in place), so the artifact can't be
    // named from the static `.SRCINFO` version in `prep.new_ver`. Extract first
    // (`--nobuild`) to freeze the real version, read the exact output filenames
    // (`makepkg --packagelist`, which now reflects the rewritten pkgver), reuse
    // them if already built, else build from the extracted sources
    // (`--noextract`). Non-VCS pkgbases have an authoritative static pkgver, so
    // a single build + version-gated collection is correct and skips the extra
    // makepkg pass.
    let (outputs, expected) = if prep.pkgbase.is_vcs() {
        makepkg::run(cfg, &prep.wt.path, &["--nobuild"], true)?;
        let expected = makepkg::package_list(cfg, &prep.wt.path)?;
        debug!(expected = ?expected, "froze package list before build");

        // Idempotency at the frozen version: reuse makepkg's exact outputs if
        // they're already on disk. Skips redundant VCS rebuilds and dodges
        // makepkg's "a package has already been built (use -f)" abort on a
        // stale artifact left by an earlier run — the phase-1 cache
        // (`prepare_one`) can't catch this since it gates on the static pkgver.
        let on_disk = select_produced(
            &install::find_produced(&prep.wt.path)?,
            &expected,
            &prep.required,
        );
        if !prep.required.is_empty() && covers_all(&on_disk, &prep.required) {
            ui::note(&format!("{}: reusing built artifact", prep.pkgbase));
            info!(pkgbase = %prep.pkgbase, files = on_disk.len(), "reusing built artifact at frozen version; skipping build");
            return Ok(on_disk);
        }

        makepkg::run(cfg, &prep.wt.path, &["--noextract"], false)?;
        let produced = install::find_produced(&prep.wt.path)?;
        (
            select_produced(&produced, &expected, &prep.required),
            Some(expected),
        )
    } else {
        makepkg::run(cfg, &prep.wt.path, &[], true)?;
        let produced = install::find_produced(&prep.wt.path)?;
        (
            select_outputs(&produced, &prep.required, &prep.new_ver),
            None,
        )
    };
    let build_secs = started.elapsed().as_secs();

    if outputs.is_empty() {
        // makepkg exited 0 yet nothing we required landed. Log the expected
        // outputs (the frozen list for VCS, the static version otherwise)
        // against what's on disk so the failure is diagnosable from the aurox
        // log, not just the TTY — this path used to return silently, leaving
        // the `selinux-refpolicy-arch-git` run showing "makepkg succeeded"
        // followed by nothing.
        error!(
            pkgbase = %prep.pkgbase,
            version = %prep.new_ver,
            expected = ?expected,
            produced = ?install::find_produced(&prep.wt.path).unwrap_or_default(),
            required = ?prep.required,
            "makepkg exited 0 but produced no matching package"
        );
        return Err(Error::Build(format!(
            "{}: makepkg produced no packages",
            prep.pkgbase
        )));
    }
    info!(
        pkgbase = %prep.pkgbase,
        version = %prep.new_ver,
        files = outputs.len(),
        build_secs,
        "build complete"
    );
    // Append the timing to the cross-session store. Failures are non-fatal:
    // the package built fine and is on disk; only the cost-visibility hint is
    // lost, and the next successful build will record again.
    record_build_metric(prep.pkgbase, build_secs);
    Ok(outputs)
}

/// Append `pkgbase`'s build duration to the metrics store. Errors are logged
/// and swallowed — see `run_build` for the rationale.
fn record_build_metric(pkgbase: &PkgBase, build_secs: u64) {
    let path = paths::metrics_db_path();
    let store = match metrics::MetricsStore::open(&path) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, path = %path.display(), "open metrics store");
            return;
        }
    };
    if let Err(e) = store.record_build(pkgbase, build_secs) {
        warn!(error = %e, %pkgbase, "record build duration");
    }
}

/// Keep only `.pkg.tar.{zst,xz}` whose `(pkgname, version)` matches one of
/// the required pkgnames at `new_ver`. Two reasons the version match is
/// load-bearing:
///   * Stale leftovers from a prior build at an older version — the
///     worktree is reused across runs, so `find_produced` returns *every*
///     historic artifact. Without the version gate, `pacman -U` would get
///     fed both versions of every selected pkgname and the install
///     transaction would either pick the wrong one or fail outright
///     (this was the google-cloud-cli 568↔569 dual-version bug).
///   * Wider prior selection — same worktree, earlier run installed more
///     siblings; the version match alone wouldn't drop them, so the
///     pkgname filter keeps the install surface honest.
fn select_outputs(files: &[PathBuf], required: &[PkgName], new_ver: &Version) -> Vec<PathBuf> {
    files
        .iter()
        .filter(|f| required.iter().any(|n| install::matches_pkg(f, n, new_ver)))
        .cloned()
        .collect()
}

/// Pick which freshly-built artifacts to install, matching against the frozen
/// `makepkg --packagelist` output (`expected`) rather than a predicted
/// version.
///
/// This is the post-build counterpart to [`select_outputs`]: where that gates
/// on a `Version` (used by the phase-1 idempotency check, which can't know a
/// VCS pkgbase's dynamic pkgver up front), this matches produced files by
/// basename against the list makepkg said it would emit — `pkgver()` already
/// evaluated. A produced file is kept iff its basename is in `expected` *and*
/// its pkgname is one the run requires, so stale artifacts from an earlier
/// build (absent from the current frozen list) and split-pkg siblings the user
/// didn't ask for are both dropped.
fn select_produced(
    produced: &[PathBuf],
    expected: &[PathBuf],
    required: &[PkgName],
) -> Vec<PathBuf> {
    use std::ffi::OsStr;
    let expected_names: HashSet<&OsStr> = expected.iter().filter_map(|p| p.file_name()).collect();
    produced
        .iter()
        .filter(|f| f.file_name().is_some_and(|n| expected_names.contains(n)))
        .filter(|f| install::extract_pkgname(f).is_some_and(|name| required.contains(&name)))
        .cloned()
        .collect()
}

/// True iff every required pkgname is represented among `selected`. Used by the
/// VCS reuse gate in `run_build` to decide whether an existing build covers the
/// whole required set (`selected` is already pkgname/frozen-filtered, so this
/// just checks coverage — the analogue of `prepare_one`'s all-present check).
fn covers_all(selected: &[PathBuf], required: &[PkgName]) -> bool {
    required.iter().all(|name| {
        selected
            .iter()
            .any(|f| install::extract_pkgname(f).as_ref() == Some(name))
    })
}

/// Entry point for `-Sc` / `-Scc`.
///
/// The depth of pacman's own cache cleanup is already encoded in `argv`;
/// aurox just wipes its per-pkgbase worktrees (idempotency cache lives
/// entirely inside them as the produced `.pkg.tar.{zst,xz}` files).
#[instrument(skip(cfg, argv))]
pub fn cmd_clean(cfg: &Config, argv: &[String]) -> Result<u8> {
    invoke::exec_pacman(cfg, argv)?;

    let pkgs_root = paths::state_dir().join("pkgs");
    if pkgs_root.exists() {
        ui::info("removing per-pkgbase worktrees");
        if let Err(e) = std::fs::remove_dir_all(&pkgs_root) {
            warn!(error = %e, "could not remove pkgs dir");
        }
        if let Err(e) = std::fs::create_dir_all(&pkgs_root) {
            warn!(error = %e, "could not recreate pkgs dir");
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert_contains;

    /// Locals so `&pkgbase` lives long enough for `blocking_dep`'s `&PkgBase`
    /// argument and return value.
    fn pb(s: &str) -> PkgBase {
        PkgBase::from(s)
    }

    /// The unknown-target hint diagnoses the missing half correctly: no
    /// suffix when full AUR data was searched, "enable it with -Sy" when the
    /// AUR is merely not set up, "it's off in config" under pacman-only mode
    /// (where -Sy would change nothing).
    #[test]
    fn unknown_targets_hint_matches_the_aur_state() {
        let ready = unknown_targets_hint("spotify".into(), index::AurState::Ready);
        assert_eq!(ready, "spotify");
        let not_set_up = unknown_targets_hint("spotify".into(), index::AurState::NotSetUp);
        assert_contains!(not_set_up, "spotify");
        assert_contains!(not_set_up, "aurox -Sy");
        let disabled = unknown_targets_hint("spotify".into(), index::AurState::Disabled);
        assert_contains!(disabled, "aur = false");
    }

    /// The setup offer fires only for an enabled-but-unsynced AUR on an
    /// interactive run: a ready index means the names are genuinely unknown,
    /// pacman-only mode is a standing choice, and neither `--noconfirm` nor
    /// a pipe has a human to answer the prompt. And it never re-asks a user
    /// who just declined the same question via `-Sy <targets>`.
    #[test]
    fn offer_applies_only_interactive_and_not_set_up() {
        use SetupOffer::{AlreadyDeclined, Open};
        assert!(offer_applies(index::AurState::NotSetUp, false, true, Open));
        assert!(!offer_applies(index::AurState::NotSetUp, true, true, Open));
        assert!(!offer_applies(
            index::AurState::NotSetUp,
            false,
            false,
            Open
        ));
        assert!(!offer_applies(index::AurState::Ready, false, true, Open));
        assert!(!offer_applies(index::AurState::Disabled, false, true, Open));
        assert!(!offer_applies(
            index::AurState::NotSetUp,
            false,
            true,
            AlreadyDeclined
        ));
    }

    /// `blocking_dep` is the resilience gate: it answers "should I skip
    /// this pkgbase because something upstream already failed?". A miss
    /// (None) means safe-to-build; a hit returns the *immediate* blocker
    /// from `make_edges`, even when the original failure is two strata
    /// back — the transitive case bottoms out because the intermediate
    /// pkgbase landed in `skipped_dep` when it was processed.
    #[test]
    fn blocking_dep_propagates_through_skipped_dep_chain() {
        let (a, b, c, standalone) = (pb("a"), pb("b"), pb("c"), pb("standalone"));
        let mut edges: HashMap<PkgBase, Vec<PkgBase>> = HashMap::new();
        edges.insert(b.clone(), vec![a.clone()]);
        edges.insert(c.clone(), vec![b.clone()]);
        let mut report = RunReport::default();

        // a failed two strata back; b skipped because of it; now check c.
        report
            .failed
            .insert(a.clone(), BuildFailure::Build("boom".into()));
        report.skipped_dep.insert(b.clone(), a.clone());

        assert_eq!(blocking_dep(&b, &edges, &report), Some(&a));
        assert_eq!(blocking_dep(&c, &edges, &report), Some(&b));
        // A pkgbase with no edges at all is always safe.
        assert_eq!(blocking_dep(&standalone, &edges, &report), None);
    }

    /// User-initiated skips block dependents identically to failures —
    /// the dep wouldn't be in localdb either way.
    #[test]
    fn blocking_dep_treats_user_skip_as_blocker() {
        let (a, b) = (pb("a"), pb("b"));
        let mut edges: HashMap<PkgBase, Vec<PkgBase>> = HashMap::new();
        edges.insert(b.clone(), vec![a.clone()]);
        let mut report = RunReport::default();
        report.skipped_user.push(a.clone());
        assert_eq!(blocking_dep(&b, &edges, &report), Some(&a));
    }

    fn pn(s: &str) -> PkgName {
        PkgName::from(s)
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Stale artifacts at an older `pkgver-pkgrel` in the same worktree
    /// must not leak into the install transaction. Regression for the
    /// google-cloud-cli 568↔569 dual-version bug: a previous build at
    /// 568 left .pkg.tar.zst files behind; the current build at 569
    /// then fed both versions into a single `pacman -U`. The version
    /// gate in `select_outputs` is what keeps the install honest.
    #[test]
    fn select_outputs_drops_stale_artifacts_at_older_version() {
        let files = vec![
            p("/wt/google-cloud-cli-bq-568.0.0-1-x86_64.pkg.tar.zst"),
            p("/wt/google-cloud-cli-bq-569.0.0-1-x86_64.pkg.tar.zst"),
        ];
        let required = vec![pn("google-cloud-cli-bq")];
        let kept = select_outputs(&files, &required, &Version::from("569.0.0-1"));
        assert_eq!(
            kept,
            vec![p("/wt/google-cloud-cli-bq-569.0.0-1-x86_64.pkg.tar.zst")],
            "older artifact must be filtered out by the version gate",
        );
    }

    /// Sibling pkgs that aren't in `required` must be filtered out even
    /// when their version matches. Mirrors the `bisq-cli` selection
    /// behaviour but at the outputs layer — the pkgname filter is what
    /// keeps a split build from installing siblings the user didn't
    /// ask for.
    #[test]
    fn select_outputs_drops_unrequested_siblings_at_matching_version() {
        let files = vec![
            p("/wt/google-cloud-cli-569.0.0-1-x86_64.pkg.tar.zst"),
            p("/wt/google-cloud-cli-bq-569.0.0-1-x86_64.pkg.tar.zst"),
            p("/wt/google-cloud-cli-gsutil-569.0.0-1-x86_64.pkg.tar.zst"),
        ];
        let required = vec![pn("google-cloud-cli-bq")];
        let kept = select_outputs(&files, &required, &Version::from("569.0.0-1"));
        assert_eq!(
            kept,
            vec![p("/wt/google-cloud-cli-bq-569.0.0-1-x86_64.pkg.tar.zst")],
        );
    }

    /// Combined: both axes filter at once. Old version of the requested
    /// pkgname AND current version of an unrequested sibling are both
    /// rejected; only `(required_pkgname, new_ver)` matches survive.
    #[test]
    fn select_outputs_filters_on_both_pkgname_and_version() {
        let files = vec![
            p("/wt/google-cloud-cli-568.0.0-1-x86_64.pkg.tar.zst"),
            p("/wt/google-cloud-cli-569.0.0-1-x86_64.pkg.tar.zst"),
            p("/wt/google-cloud-cli-bq-568.0.0-1-x86_64.pkg.tar.zst"),
            p("/wt/google-cloud-cli-bq-569.0.0-1-x86_64.pkg.tar.zst"),
            p("/wt/google-cloud-cli-gsutil-569.0.0-1-x86_64.pkg.tar.zst"),
        ];
        let required = vec![pn("google-cloud-cli-bq")];
        let kept = select_outputs(&files, &required, &Version::from("569.0.0-1"));
        assert_eq!(
            kept,
            vec![p("/wt/google-cloud-cli-bq-569.0.0-1-x86_64.pkg.tar.zst")],
            "only the (required_pkgname, new_ver) artifact survives both filters",
        );
    }

    /// Full-selection non-split case: `required` is the entry's entire
    /// pkgname list, and every artifact at `new_ver` passes through.
    /// Pins that the version gate doesn't accidentally drop valid
    /// outputs when there's no subset.
    #[test]
    fn select_outputs_passes_every_required_pkg_at_matching_version() {
        let files = vec![p("/wt/test-trivial-1.0-1-any.pkg.tar.zst")];
        let required = vec![pn("test-trivial")];
        let kept = select_outputs(&files, &required, &Version::from("1.0-1"));
        assert_eq!(kept, files);
    }

    /// Regression for the `selinux-refpolicy-arch-git` failure: a VCS pkgbase's
    /// `pkgver()` stamps a *dynamic* version (`…r70.g0dae0f47c-1`) that the
    /// static `.SRCINFO` pkgver (`…r2.g3e316c1c5-2`, what landed in
    /// `prep.new_ver`) never matches. Collecting against the frozen
    /// `makepkg --packagelist` output keeps the real artifact, where the old
    /// version-gated path returned empty and failed with "produced no
    /// packages" despite a clean makepkg exit.
    #[test]
    fn select_produced_keeps_vcs_artifact_at_dynamic_pkgver() {
        let built =
            "/wt/selinux-refpolicy-arch-git-RELEASE_2_20260312.r70.g0dae0f47c-1-any.pkg.tar.zst";
        let produced = vec![p(built)];
        // The frozen list carries the same dynamic version — the static r2
        // pkgver never appears here.
        let expected = vec![p(built)];
        let required = vec![pn("selinux-refpolicy-arch-git")];
        let kept = select_produced(&produced, &expected, &required);
        assert_eq!(
            kept,
            vec![p(built)],
            "VCS artifact must be collected by its frozen pkgver",
        );
    }

    /// Companion to the regression above, pinning *why* the freeze is needed:
    /// gating the same VCS artifact on the static `.SRCINFO` version (what
    /// `select_outputs` does) drops it — the exact bug `select_produced`
    /// avoids.
    #[test]
    fn select_outputs_static_version_drops_vcs_artifact() {
        let built = vec![p(
            "/wt/selinux-refpolicy-arch-git-RELEASE_2_20260312.r70.g0dae0f47c-1-any.pkg.tar.zst",
        )];
        let required = vec![pn("selinux-refpolicy-arch-git")];
        let static_ver = Version::from("RELEASE_2_20260312.r2.g3e316c1c5-2");
        let kept = select_outputs(&built, &required, &static_ver);
        assert!(
            kept.is_empty(),
            "static-version gate cannot match the dynamic built version — this is the bug",
        );
    }

    /// `select_produced` still drops what isn't in the frozen list (a stale
    /// artifact from an earlier build at a different pkgver) and what isn't
    /// required (a split-pkg sibling), matching `select_outputs`' two filters
    /// but keyed on the frozen filenames instead of a predicted version.
    #[test]
    fn select_produced_drops_stale_and_unrequested_siblings() {
        let produced = vec![
            p("/wt/pkg-a-2.0-1-any.pkg.tar.zst"), // current + required
            p("/wt/pkg-a-1.0-1-any.pkg.tar.zst"), // stale: not in frozen list
            p("/wt/pkg-b-2.0-1-any.pkg.tar.zst"), // sibling: frozen but not required
        ];
        let expected = vec![
            p("/wt/pkg-a-2.0-1-any.pkg.tar.zst"),
            p("/wt/pkg-b-2.0-1-any.pkg.tar.zst"),
        ];
        let required = vec![pn("pkg-a")];
        let kept = select_produced(&produced, &expected, &required);
        assert_eq!(kept, vec![p("/wt/pkg-a-2.0-1-any.pkg.tar.zst")]);
    }

    /// `artifacts_built` scopes to whatever pkgname set the caller passes — the
    /// distinction the picker relies on. A split pkgbase whose worktree holds
    /// only *one* member's artifact reads built for that member (the per-row
    /// query) but not for the whole pkgbase (the dep-row query). The matching
    /// version must be exact, and a never-built pkgbase is never built.
    #[test]
    fn artifacts_built_scopes_to_requested_pkgnames() {
        use crate::testing::ScopedStateRoot;

        let tmp = tempfile::tempdir().unwrap();
        let _root = ScopedStateRoot::new(tmp.path().to_path_buf());

        // A split pkgbase `splitpkg` producing `splitpkg-a` and `splitpkg-b`,
        // but only `-a`'s artifact is on disk at 2.0-1.
        let pkgbase = pb("splitpkg");
        let wt = paths::pkg_worktree(&pkgbase);
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join("splitpkg-a-2.0-1-x86_64.pkg.tar.zst"), "").unwrap();
        let ver = Version::from("2.0-1");

        // Per-row (single pkgname): the built member reads built, its sibling
        // does not — each split row gets its own answer.
        assert!(artifacts_built(&pkgbase, &ver, &[pn("splitpkg-a")]));
        assert!(!artifacts_built(&pkgbase, &ver, &[pn("splitpkg-b")]));
        // Whole-pkgbase (dep row): not done until every member is present.
        assert!(!artifacts_built(
            &pkgbase,
            &ver,
            &[pn("splitpkg-a"), pn("splitpkg-b")]
        ));

        // Version must match exactly, and an empty set is never built.
        assert!(!artifacts_built(
            &pkgbase,
            &Version::from("2.0-2"),
            &[pn("splitpkg-a")]
        ));
        assert!(!artifacts_built(&pkgbase, &ver, &[]));
        // A pkgbase with no worktree at all is simply not built.
        assert!(!artifacts_built(&pb("ghost"), &ver, &[pn("ghost")]));
    }
}
