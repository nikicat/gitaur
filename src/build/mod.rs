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
use crate::index::{self, IndexEntry, IndexFile};
use crate::mirror::{self, MirrorRepo};
use crate::pacman::alpm_db::{self, PacmanIndex};
use crate::pacman::invoke::PkgUpgrade;
use crate::pacman::{invoke, vercmp};
use crate::paths;
use crate::resolver::{self, Plan};
use crate::ui;
use std::collections::HashSet;
use std::path::PathBuf;
use tracing::{debug, info, instrument, warn};

pub mod install;
pub mod makepkg;
pub mod review;

/// One built pkgbase's set of `.pkg.tar.zst` outputs.
struct BuiltPkg {
    pkgbase: String,
    files: Vec<PathBuf>,
}

/// Render the resolved [`Plan`] to stderr as aligned `name  version` tables
/// — one group per source — mirroring the style of [`ui::upgrade_table`] used
/// by `-Su`. Versions are looked up live from `pac` (sync DBs) and `idx`
/// (AUR index), so the plan answers "which exact version would land?" for
/// every row before the user confirms.
fn print_plan(plan: &Plan, idx: &IndexFile, pac: &PacmanIndex) {
    if plan.direct_repo.is_empty() && plan.transitive_repo.is_empty() && plan.aur_strata.is_empty()
    {
        ui::info("plan: nothing to do");
        return;
    }
    if !plan.direct_repo.is_empty() {
        ui::install_table(
            "Repo packages (explicit)",
            &rows_for_repo(&plan.direct_repo, pac),
        );
    }
    if !plan.transitive_repo.is_empty() {
        ui::install_table(
            "Repo dependencies",
            &rows_for_repo(&plan.transitive_repo, pac),
        );
    }
    if !plan.aur_strata.is_empty() {
        let total = plan.aur_strata.len();
        if total == 1 {
            ui::install_table("AUR build order", &rows_for_aur(&plan.aur_strata[0], idx));
        } else {
            for (i, stratum) in plan.aur_strata.iter().enumerate() {
                ui::install_table(
                    &format!("AUR build stratum {}/{total}", i + 1),
                    &rows_for_aur(stratum, idx),
                );
            }
        }
    }
}

/// Pair each repo pkgname with its sync-repo version. A name that only
/// matched via a virtual `provides` won't carry a version of its own (pacman
/// will choose a concrete provider at install time); render an empty version
/// cell rather than guessing.
fn rows_for_repo(names: &[String], pac: &PacmanIndex) -> Vec<(String, String)> {
    names
        .iter()
        .map(|n| (n.clone(), pac.sync_version(n).unwrap_or("").to_string()))
        .collect()
}

/// Pair each AUR pkgbase with its index version (`[epoch:]pkgver-pkgrel`).
/// All pkgnames in a split pkgbase share that version, so the pkgbase row
/// is unambiguous even when only a subset of pkgnames will be installed.
fn rows_for_aur(pkgbases: &[String], idx: &IndexFile) -> Vec<(String, String)> {
    pkgbases
        .iter()
        .map(|pb| {
            let ver = idx
                .entries
                .iter()
                .find(|e| e.pkgbase == *pb)
                .map(IndexEntry::version)
                .unwrap_or_default();
            (pb.clone(), ver)
        })
        .collect()
}

/// Entry point for `gitaur -S <targets>`.
///
/// Loads the pacman snapshot and (optionally) the AUR index in parallel, then
/// classifies every target. After printing the unified plan and getting a
/// single confirmation gitaur drives every `pacman` call with `--noconfirm`
/// so the user is asked once; pacman never re-prompts. `already_confirmed`
/// short-circuits the gate for callers that have already confirmed at a
/// higher level (e.g. the `-Syu` interactive picker in `cli::dispatch`);
/// PKGBUILD review prompts still respect `noconfirm`.
#[instrument(skip(cfg))]
pub fn cmd_install(
    cfg: &Config,
    targets: &[String],
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
        || -> Result<Option<(IndexFile, Secondary)>> {
            if !idx_path.exists() {
                return Ok(None);
            }
            let idx = index::load(&idx_path)?;
            let by = Secondary::build(&idx);
            Ok(Some((idx, by)))
        },
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

    // Expand bare `-S <pkgbase>` targets into the pkgname(s) the user wants
    // installed as explicit. Split pkgbases prompt for a subset; single-pkgname
    // pkgbases pass through silently. The selector closure delegates to
    // `ui::select_pkgnames` so tests can swap in a deterministic picker.
    let expanded = resolver::expand_pkgbase_targets(idx, by, &pac, targets, &mut |pb, pns| {
        ui::select_pkgnames(pb, pns, noconfirm).map_err(|e| Error::other(e.to_string()))
    })?;
    let mut plan = resolver::resolve(cfg, idx, by, &pac, &expanded.targets)?;
    plan.pkgname_selections = expanded.selections;
    // For pkgbase/provides hits the resolver received the pkgbase string, so
    // `plan.direct_targets` only contains the pkgbase. Mark the pkgnames the
    // user actually chose as direct too, so `install_stratum` flags their
    // `.pkg.tar.zst` Explicit instead of `--asdeps`.
    plan.direct_targets.extend(expanded.direct_pkgnames);

    print_plan(&plan, idx, &pac);

    if plan.direct_repo.is_empty() && plan.transitive_repo.is_empty() && plan.aur_strata.is_empty()
    {
        return Ok(0);
    }

    if !already_confirmed && !ui::confirm("Proceed with installation?", noconfirm)? {
        return Err(Error::UserAbort);
    }

    install_repo_phase(cfg, &plan, asdeps)?;

    if !plan.aur_strata.is_empty() {
        // AUR path needs a loaded index — by construction `aur_strata` is
        // empty when `by == None`, so this unwrap is unreachable.
        let idx = aur_loaded
            .as_ref()
            .map(|(i, _)| i)
            .ok_or_else(|| Error::other("internal: AUR plan without index"))?;
        run_aur_pipeline(cfg, idx, &pac, &plan, noconfirm, asdeps)?;
    }
    Ok(0)
}

/// Install the user's repo targets up front: direct ones as explicit, deps
/// as `--asdeps`. Two `pacman -S` calls so the install-reason flag is per-
/// batch; sudo cache bridges them. No-op when both buckets are empty.
/// Always `--noconfirm`: gitaur already gated this with its own prompt, so
/// pacman shouldn't ask again.
fn install_repo_phase(cfg: &Config, plan: &Plan, asdeps: bool) -> Result<()> {
    if !plan.direct_repo.is_empty() {
        ui::info("installing repo packages");
        let mut args = vec!["-S".to_string(), "--needed".into(), "--noconfirm".into()];
        if asdeps {
            args.push("--asdeps".into());
        }
        args.extend(plan.direct_repo.iter().cloned());
        invoke::exec_pacman(cfg, &args)?;
    }
    if !plan.transitive_repo.is_empty() {
        ui::info("installing repo dependencies");
        let mut args = vec![
            "-S".to_string(),
            "--needed".into(),
            "--noconfirm".into(),
            "--asdeps".into(),
        ];
        args.extend(plan.transitive_repo.iter().cloned());
        invoke::exec_pacman(cfg, &args)?;
    }
    Ok(())
}

/// Stratified AUR build+install loop.
///
/// For each stratum (set of AUR pkgbases whose build-time deps are all in
/// earlier strata): build every pkgbase, then `pacman -U` the resulting
/// `.pkg.tar.zst`'s so the next stratum's `makepkg` finds them in localdb.
/// Sudo cache (typically 5-15 min) bridges per-stratum sudo prompts. Plain
/// runtime `depends` are resolved by the final stratum's `pacman -U`
/// resolving against the same batch. After all strata, transitive AUR pkgs
/// that ended up Explicit during their stratum's `-U` are flipped to
/// `--asdeps` via a single cheap `pacman -D` call.
fn run_aur_pipeline(
    cfg: &Config,
    idx: &IndexFile,
    pac: &PacmanIndex,
    plan: &Plan,
    noconfirm: bool,
    asdeps: bool,
) -> Result<()> {
    let mirror = MirrorRepo::open(&paths::aur_repo_path())?;
    let direct_names: HashSet<&str> = plan
        .direct_targets
        .iter()
        .map(std::string::String::as_str)
        .collect();
    let mut transitive_marks: Vec<String> = Vec::new();

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
            let selection = plan.pkgname_selections.get(pkgbase).map(Vec::as_slice);
            row.push(prepare_one(
                &mirror, idx, pac, pkgbase, selection, noconfirm,
            )?);
        }
        prep_strata.push(row);
    }

    // Phase 2: makepkg approved pkgbases, install per-stratum so later
    // strata's makepkg finds earlier strata's deps in localdb.
    for (stratum_idx, (stratum, preps)) in
        plan.aur_strata.iter().zip(prep_strata).enumerate()
    {
        if plan.aur_strata.len() > 1 {
            ui::info(&format!(
                "build stratum {}/{}: {}",
                stratum_idx + 1,
                plan.aur_strata.len(),
                stratum.join(" "),
            ));
        }
        let mut stratum_built: Vec<BuiltPkg> = Vec::with_capacity(preps.len());
        for prep in preps {
            match prep.disposition {
                Disposition::Skipped => {
                    ui::note(&format!("{}: skipped", prep.pkgbase));
                }
                Disposition::Cached(files) => {
                    stratum_built.push(BuiltPkg {
                        pkgbase: prep.pkgbase.to_owned(),
                        files,
                    });
                }
                Disposition::Build => {
                    let files = run_build(cfg, &prep)?;
                    stratum_built.push(BuiltPkg {
                        pkgbase: prep.pkgbase.to_owned(),
                        files,
                    });
                }
            }
        }
        install_stratum(
            cfg,
            idx,
            &stratum_built,
            &direct_names,
            asdeps,
            &mut transitive_marks,
        )?;
    }

    if !asdeps && !transitive_marks.is_empty() {
        let mut args = vec!["-D".to_string(), "--asdeps".into()];
        args.extend(transitive_marks);
        invoke::exec_pacman(cfg, &args)?;
    }
    Ok(())
}

/// One pkgbase's prepared state, produced in phase 1 and consumed in phase 2.
struct Prep<'a> {
    pkgbase: &'a str,
    wt: mirror::worktree::Worktree,
    new_ver: String,
    selection: Option<&'a [String]>,
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

#[instrument(skip(mirror, idx, pac, selection))]
fn prepare_one<'a>(
    mirror: &MirrorRepo,
    idx: &'a IndexFile,
    pac: &PacmanIndex,
    pkgbase: &'a str,
    selection: Option<&'a [String]>,
    noconfirm: bool,
) -> Result<Prep<'a>> {
    let entry = idx
        .entries
        .iter()
        .find(|e| e.pkgbase == pkgbase)
        .ok_or_else(|| Error::Build(format!("{pkgbase}: missing from index")))?;
    let dest = paths::pkg_worktree(pkgbase);
    let wt = mirror::worktree::add_or_reset(mirror, pkgbase, &dest)?;

    let new_ver = entry.version();
    let required: Vec<&str> = match selection {
        Some(sel) => sel.iter().map(String::as_str).collect(),
        None => entry.pkgnames.iter().map(|p| p.name.as_str()).collect(),
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
        let kept = filter_by_selection(&existing, selection);
        ui::note(&format!("{pkgbase}: already built {new_ver}"));
        debug!(
            pkgbase,
            version = %new_ver,
            files = kept.len(),
            "reusing cached build"
        );
        return Ok(Prep {
            pkgbase,
            wt,
            new_ver,
            selection,
            disposition: Disposition::Cached(kept),
        });
    }

    let installed_ver = entry
        .pkgnames
        .iter()
        .find_map(|p| pac.installed_version(&p.name));
    let disposition = match review::review(
        mirror,
        pkgbase,
        &new_ver,
        installed_ver,
        &wt,
        noconfirm,
    )? {
        review::Outcome::Approved => Disposition::Build,
        review::Outcome::Skipped => Disposition::Skipped,
    };
    Ok(Prep {
        pkgbase,
        wt,
        new_ver,
        selection,
        disposition,
    })
}

#[instrument(skip(cfg, prep), fields(pkgbase = prep.pkgbase, version = %prep.new_ver))]
fn run_build(cfg: &Config, prep: &Prep) -> Result<Vec<PathBuf>> {
    ui::step(&format!("makepkg {}", prep.pkgbase));
    makepkg::run(cfg, &prep.wt.path)?;

    let produced = install::find_produced(&prep.wt.path)?;
    let outputs = filter_by_selection(&produced, prep.selection);
    if outputs.is_empty() {
        return Err(Error::Build(format!(
            "{}: makepkg produced no packages",
            prep.pkgbase
        )));
    }
    info!(
        pkgbase = prep.pkgbase,
        version = %prep.new_ver,
        files = outputs.len(),
        "build complete"
    );
    Ok(outputs)
}

/// Keep only `.pkg.tar.zst` whose pkgname is in `selection`. `None` means no
/// filter (default for non-split builds and dependency builds). Guards
/// against stale leftover files (e.g. a prior wider build) when reusing a
/// cached build.
fn filter_by_selection(files: &[PathBuf], selection: Option<&[String]>) -> Vec<PathBuf> {
    let Some(sel) = selection else {
        return files.to_vec();
    };
    files
        .iter()
        .filter(|f| install::extract_pkgname(f).is_some_and(|n| sel.iter().any(|s| s == &n)))
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
    direct: &HashSet<&str>,
    asdeps_override: bool,
    transitive_marks: &mut Vec<String>,
) -> Result<()> {
    if built.is_empty() {
        return Ok(());
    }
    let total: usize = built.iter().map(|b| b.files.len()).sum();
    ui::step(&format!("installing {total} built package(s) with pacman"));

    let mut files: Vec<PathBuf> = Vec::new();
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
            let pkgname = install::extract_pkgname(f).unwrap_or_default();
            let is_direct = !asdeps_override && direct.contains(pkgname.as_str());
            if !is_direct {
                transitive_marks.push(pkgname);
            }
        }
    }

    // Always `--noconfirm`: gitaur's plan+confirm at the top of `cmd_install`
    // is the single user gate; pacman shouldn't ask again.
    let mut args = vec!["-U".to_string(), "--needed".into(), "--noconfirm".into()];
    if asdeps_override {
        args.push("--asdeps".into());
    }
    for p in &files {
        args.push(p.to_string_lossy().into_owned());
    }
    invoke::exec_pacman(cfg, &args)?;
    Ok(())
}

/// Scan the localdb for foreign pkgs with a newer version in the AUR index.
///
/// `devel=true` forces every VCS pkgbase (`-git`/`-svn`/`-hg`/`-bzr`) into
/// the list regardless of vercmp, since their `pkgver` is only refreshed by
/// `makepkg`. Otherwise VCS pkgs are skipped (their on-disk version always
/// looks stale).
fn aur_upgrades(
    idx: &IndexFile,
    by: &Secondary,
    pac: &PacmanIndex,
    devel: bool,
) -> Vec<PkgUpgrade> {
    let mut out = Vec::new();
    for (name, installed_ver) in pac.foreign() {
        let Some(entry) = by.lookup(idx, &name) else {
            warn!(name, "foreign pkg not in AUR index");
            continue;
        };
        let is_vcs = is_vcs_pkg(&entry.pkgbase);
        if !devel && is_vcs {
            continue;
        }
        let aur_ver = entry.version();
        let need = (devel && is_vcs) || vercmp::is_outdated(&installed_ver, &aur_ver);
        if need {
            out.push(PkgUpgrade {
                repo: invoke::REPO_AUR.into(),
                name,
                old_ver: installed_ver,
                new_ver: aur_ver,
            });
        }
    }
    out
}

/// `gitaur -Qu` — show the union of pacman-repo and AUR upgrade candidates
/// in one flat, severity-sorted table grouped by `repo` column. Read-only
/// and unprivileged (no sudo), so safe to call both as the bare `-Qu` and
/// as a preview before `-Syu` runs.
#[instrument]
pub fn cmd_query_upgrades(devel: bool) -> Result<u8> {
    ui::upgrade_table(&collect_upgrade_plan(devel)?);
    Ok(0)
}

/// Gather the merged repo + AUR upgrade list. Shared by `-Qu` (read-only
/// rendering) and `-Syu` (feeds the interactive picker). Unprivileged —
/// reads alpm and the AUR index file only.
pub fn collect_upgrade_plan(devel: bool) -> Result<Vec<PkgUpgrade>> {
    let mut plan = invoke::query_repo_upgrades()?;
    let idx_path = paths::index_path();
    if idx_path.exists() {
        let idx = index::load(&idx_path)?;
        let by = Secondary::build(&idx);
        let alpm = alpm_db::open()?;
        let pac = PacmanIndex::build(&alpm);
        plan.extend(aur_upgrades(&idx, &by, &pac, devel));
    }
    Ok(plan)
}

/// Entry point for `-Sc` / `-Scc`. The depth of pacman's own cache cleanup is
/// already encoded in `argv`; gitaur just wipes its per-pkgbase worktrees
/// (idempotency cache lives entirely inside them as the produced
/// `.pkg.tar.{zst,xz}` files).
#[instrument(skip(cfg, argv))]
pub fn cmd_clean(cfg: &Config, argv: &[String]) -> Result<u8> {
    invoke::exec_pacman(cfg, argv)?;

    let pkgs_root = paths::state_dir().join("pkgs");
    if pkgs_root.exists() {
        ui::info("removing per-pkgbase worktrees");
        if let Err(e) = std::fs::remove_dir_all(&pkgs_root) {
            warn!(error = %e, "could not remove pkgs dir");
        }
        let _ = std::fs::create_dir_all(&pkgs_root);
    }
    Ok(0)
}

fn is_vcs_pkg(pkgbase: &str) -> bool {
    pkgbase.ends_with("-git")
        || pkgbase.ends_with("-svn")
        || pkgbase.ends_with("-hg")
        || pkgbase.ends_with("-bzr")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_vcs_suffixes() {
        assert!(is_vcs_pkg("neovim-git"));
        assert!(is_vcs_pkg("foo-svn"));
        assert!(is_vcs_pkg("bar-hg"));
        assert!(is_vcs_pkg("baz-bzr"));
        assert!(!is_vcs_pkg("neovim"));
        assert!(!is_vcs_pkg("git-lfs"));
    }
}
