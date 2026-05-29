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
use crate::error::{Error, Result};
use crate::index::secondary::Secondary;
use crate::index::{self, IndexFile};
use crate::mirror::{self, MirrorRepo};
use crate::names::{PkgBase, PkgName, PkgTarget, PkgTargetSetExt};
use crate::pacman::alpm_db::{self, PacmanIndex};
use crate::pacman::invoke;
use crate::paths;
use crate::resolver::{self, PkgbasePlan, Plan};
use crate::runopts;
use crate::ui;
use crate::version::Version;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use tracing::{debug, info, instrument, warn};

pub mod install;
pub mod makepkg;
pub mod print;
pub mod review;
pub mod upgrade;

pub use upgrade::{UpgradeSession, cmd_query_upgrades, collect_upgrade_plan};

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

/// Entry point for `gaur -S <targets>`.
///
/// Loads the pacman snapshot and (optionally) the AUR index in parallel, then
/// hands them to [`install_with_index`]. After printing the unified plan and
/// getting a single confirmation gitaur drives every `pacman` call with
/// `--noconfirm` so the user is asked once; pacman never re-prompts.
#[instrument(skip(cfg))]
pub fn cmd_install(
    cfg: &Config,
    targets: &[Target],
    noconfirm: bool,
    asdeps: bool,
    already_confirmed: bool,
) -> Result<u8> {
    let idx_path = paths::index_path();

    // Pacman snapshot + AUR index loaded concurrently. PacmanIndex iterates
    // every sync DB and the localdb (tens of ms on a typical system); the
    // AUR mmap + rkyv deserialize is similar. rayon::join hides one behind
    // the other.
    let (pac_res, idx_res) = rayon::join(
        || -> Result<PacmanIndex> {
            let alpm = alpm_db::open()?;
            Ok(PacmanIndex::build(&alpm))
        },
        // `propagate` so `load_or_resync` sees `--noresync` even when rayon
        // runs this closure on a worker thread (its RunOpts TLS is otherwise
        // the default).
        runopts::propagate(|| -> Result<Option<(IndexFile, Secondary)>> {
            if !idx_path.exists() {
                return Ok(None);
            }
            let idx = index::load_or_resync(cfg, &idx_path)?;
            let by = Secondary::build(&idx);
            Ok(Some((idx, by)))
        }),
    );
    let pac = pac_res?;
    let aur_loaded = idx_res?;

    let empty_idx;
    let (idx, by): (&IndexFile, Option<&Secondary>) = if let Some((i, s)) = aur_loaded.as_ref() {
        (i, Some(s))
    } else {
        empty_idx = IndexFile::empty();
        (&empty_idx, None)
    };

    // `-S` has no cross-call review memory, so a fresh per-invocation set. The
    // upgrade loop keeps its own session-scoped set across batches instead.
    let mut reviewed = HashSet::new();
    let opts = InstallOpts {
        noconfirm,
        asdeps,
        gate: if already_confirmed {
            ConfirmGate::AlreadyConfirmed
        } else {
            ConfirmGate::Ask
        },
    };
    let report = install_with_index(cfg, idx, by, &pac, targets, opts, &mut reviewed)?;
    // A Ctrl+C'd build exits non-zero too: the user aborted, so `||` chains and
    // scripts should see the run didn't complete.
    Ok(u8::from(
        report.had_failures() || !report.interrupted.is_empty(),
    ))
}

/// Resolve `targets` against a caller-supplied index + pacman snapshot, then
/// drive the repo and AUR install phases, returning the per-pkgbase
/// [`RunReport`].
///
/// Split out of [`cmd_install`] so the no-arg upgrade loop can run many
/// batches against one already-loaded [`IndexFile`]/[`Secondary`] without
/// re-reading the index each iteration, and so the loop can fold the report
/// into its session state. `reviewed` is the session-scoped set of pkgbases
/// already approved (see [`prepare_one`]); `-S` passes a fresh empty one.
pub(crate) fn install_with_index(
    cfg: &Config,
    idx: &IndexFile,
    by: Option<&Secondary>,
    pac: &PacmanIndex,
    targets: &[Target],
    opts: InstallOpts,
    reviewed: &mut HashSet<PkgBase>,
) -> Result<RunReport> {
    let plan = resolve_targets(cfg, idx, by, pac, targets, opts.noconfirm)?;
    print::plan(&plan, idx, pac);
    apply_plan(cfg, idx, pac, &plan, opts, reviewed)
}

/// Classify `targets` into a [`Plan`] — expand bare pkgbases, resolve deps, and
/// stamp the per-pkgbase decisions (split selections, counterpart hints) onto
/// it.
///
/// Pulled out of [`install_with_index`] so the upgrade loop can resolve once,
/// render its change-set preview against the [`Plan`], and then hand the *same*
/// plan to [`apply_plan`] — re-resolving would re-run the split-package prompt
/// inside `expand_pkgbase_targets`.
pub(crate) fn resolve_targets(
    cfg: &Config,
    idx: &IndexFile,
    by: Option<&Secondary>,
    pac: &PacmanIndex,
    targets: &[Target],
    noconfirm: bool,
) -> Result<Plan> {
    // Expand bare `-S <pkgbase>` targets into the pkgname(s) the user wants
    // installed as explicit. Split pkgbases prompt for a subset; single-pkgname
    // pkgbases pass through silently. The selector closure delegates to
    // `ui::select_pkgnames` so tests can swap in a deterministic picker.
    let expanded = resolver::expand_pkgbase_targets(idx, by, pac, targets, &mut |pb, pns| {
        ui::select_pkgnames(pb, pns, noconfirm).map_err(|e| Error::other(e.to_string()))
    })?;
    let mut plan = resolver::resolve(cfg, idx, by, pac, &expanded.targets)?;
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
    cfg: &Config,
    idx: &IndexFile,
    pac: &PacmanIndex,
    plan: &Plan,
    opts: InstallOpts,
    reviewed: &mut HashSet<PkgBase>,
) -> Result<RunReport> {
    if plan.direct_repo.is_empty() && plan.transitive_repo.is_empty() && plan.aur_strata.is_empty()
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

    install_repo_phase(cfg, plan, opts.asdeps)?;

    if plan.aur_strata.is_empty() {
        return Ok(RunReport::default());
    }
    // A non-empty `aur_strata` implies the resolver found AUR entries, which
    // can only happen when `by` (and thus a real `idx`) was present — so `idx`
    // here is the loaded index, never the empty fallback.
    run_aur_pipeline(cfg, idx, pac, plan, opts, reviewed)
}

/// Install the user's repo targets up front: direct ones as explicit, deps
/// as `--asdeps`. Two `pacman -S` calls so the install-reason flag is per-
/// batch; sudo cache bridges them. No-op when both buckets are empty.
/// Always `--noconfirm`: gitaur already gated this with its own prompt, so
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
    cfg: &Config,
    idx: &IndexFile,
    pac: &PacmanIndex,
    plan: &Plan,
    opts: InstallOpts,
    reviewed: &mut HashSet<PkgBase>,
) -> Result<RunReport> {
    let mirror = MirrorRepo::open(&paths::aur_repo_path())?;
    // `plan.direct_targets` is already `HashSet<PkgTarget>` — pass it
    // through; `install_stratum` uses `PkgTargetSetExt::contains_pkgname`
    // to test built pkgs without any string-level cast at the call site.
    let direct_names = &plan.direct_targets;
    let mut transitive_marks: Vec<PkgName> = Vec::new();

    // Phase 1: open every worktree, run idempotency checks, and prompt the
    // user for review across all strata up front. Skipped pkgbases are
    // dropped; an "abort" propagates immediately as Error::UserAbort. No
    // makepkg runs in this phase, so the user can walk through every diff
    // before any build kicks off.
    let mut prep_strata: Vec<Vec<Prep<'_>>> = Vec::with_capacity(plan.aur_strata.len());
    for stratum in &plan.aur_strata {
        let mut row = Vec::with_capacity(stratum.len());
        for pkgbase in stratum {
            // Partial-split selection — present only when the user asked
            // for a subset. makepkg always packages every pkgname in a
            // split (no `--pkg=` flag); we filter the produced files down
            // to the selection so `install_stratum`'s `pacman -U` skips
            // the rest.
            row.push(prepare_one(
                &mirror,
                idx,
                pac,
                &plan.pkgbase_plan(pkgbase),
                cfg.review_history_scan_max,
                opts.noconfirm,
                reviewed,
            )?);
        }
        prep_strata.push(row);
    }

    let mut report = RunReport::default();

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
        let built = build_stratum(cfg, preps, &plan.aur_make_edges, &mut report);
        commit_stratum(
            cfg,
            idx,
            &built,
            stratum_idx,
            direct_names,
            opts.asdeps,
            &mut transitive_marks,
            &mut report,
        );
        // A Ctrl+C'd build stops the whole batch here — anything already
        // built+installed above stays; the rest stays outstanding for the next
        // table pass (or a `-S` retry).
        if !report.interrupted.is_empty() {
            ui::warn("build interrupted — stopping this batch");
            break;
        }
    }

    if !opts.asdeps && !transitive_marks.is_empty() {
        let mut args = vec!["-D".to_owned(), "--asdeps".into()];
        // pacman argv is `Vec<String>` — downgrade typed `PkgName`s at
        // this single boundary.
        args.extend(transitive_marks.into_iter().map(PkgName::into_inner));
        if let Err(e) = invoke::exec_pacman(cfg, &args) {
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
    cfg: &Config,
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
            Disposition::Build => match run_build(cfg, &prep) {
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

/// Run `pacman -U` for one stratum's built pkgs and update `report` with the
/// outcome. A pacman failure is atomic, so every pkgbase in this stratum is
/// marked failed and the next stratum's dep check skips dependents.
#[allow(clippy::too_many_arguments)]
fn commit_stratum(
    cfg: &Config,
    idx: &IndexFile,
    built: &[BuiltPkg],
    stratum_idx: usize,
    direct: &HashSet<PkgTarget>,
    asdeps_override: bool,
    transitive_marks: &mut Vec<PkgName>,
    report: &mut RunReport,
) {
    if built.is_empty() {
        return;
    }
    match install_stratum(cfg, idx, built, direct, asdeps_override, transitive_marks) {
        Ok(()) => {
            for b in built {
                report.installed.push(b.pkgbase.clone());
            }
        }
        Err(e) => {
            let msg = e.to_string();
            ui::error(&format!(
                "stratum {} install failed: {msg}",
                stratum_idx + 1
            ));
            for b in built {
                report
                    .failed
                    .insert(b.pkgbase.clone(), BuildFailure::Install(msg.clone()));
            }
        }
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

#[instrument(skip(mirror, idx, pac, target, reviewed), fields(pkgbase = %target.pkgbase))]
fn prepare_one<'a>(
    mirror: &MirrorRepo,
    idx: &'a IndexFile,
    pac: &PacmanIndex,
    target: &PkgbasePlan<'a>,
    history_scan_max: usize,
    noconfirm: bool,
    reviewed: &mut HashSet<PkgBase>,
) -> Result<Prep<'a>> {
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
    let disposition = match review::review(
        mirror,
        pkgbase,
        &new_ver,
        counterpart.as_ref(),
        &wt,
        history_scan_max,
        noconfirm,
    )? {
        review::Outcome::Approved => {
            // Remember the approval so a later batch in the same session
            // doesn't re-prompt the same diff.
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

#[instrument(skip(cfg, prep), fields(pkgbase = %prep.pkgbase, version = %prep.new_ver))]
fn run_build(cfg: &Config, prep: &Prep<'_>) -> Result<Vec<PathBuf>> {
    ui::step(&format!("makepkg {}", prep.pkgbase));
    makepkg::run(cfg, &prep.wt.path)?;

    let produced = install::find_produced(&prep.wt.path)?;
    let outputs = select_outputs(&produced, &prep.required, &prep.new_ver);
    if outputs.is_empty() {
        return Err(Error::Build(format!(
            "{}: makepkg produced no packages",
            prep.pkgbase
        )));
    }
    info!(
        pkgbase = %prep.pkgbase,
        version = %prep.new_ver,
        files = outputs.len(),
        "build complete"
    );
    Ok(outputs)
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

/// Install every `.pkg.tar.zst` produced by one stratum's builds in a single
/// `pacman -U` transaction so intra-stratum runtime deps (split packages,
/// AUR pkg + sibling AUR dep) resolve against each other. Pkgnames that
/// weren't on the user's command line are appended to `transitive_marks` so
/// the caller can flip them to `--asdeps` at the very end.
#[instrument(skip(cfg, idx, built, direct, transitive_marks))]
fn install_stratum(
    cfg: &Config,
    idx: &IndexFile,
    built: &[BuiltPkg],
    direct: &HashSet<PkgTarget>,
    asdeps_override: bool,
    transitive_marks: &mut Vec<PkgName>,
) -> Result<()> {
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
            let is_direct = !asdeps_override && direct.contains_pkgname(&pkgname);
            if !is_direct {
                pending_marks.push(pkgname);
            }
        }
    }

    // Always `--noconfirm`: gitaur's plan+confirm at the top of `cmd_install`
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
    transitive_marks.extend(pending_marks);
    Ok(())
}

/// Entry point for `-Sc` / `-Scc`.
///
/// The depth of pacman's own cache cleanup is already encoded in `argv`;
/// gitaur just wipes its per-pkgbase worktrees (idempotency cache lives
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

    /// Locals so `&pkgbase` lives long enough for `blocking_dep`'s `&PkgBase`
    /// argument and return value.
    fn pb(s: &str) -> PkgBase {
        PkgBase::from(s)
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
}
