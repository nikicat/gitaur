//! Build orchestration: plan → batched repo deps → unprivileged build loop → final batched install.
//!
//! Sudo is deferred to the very end and prompted exactly once for the `pacman -U`
//! step. Builds are idempotent: a pkgbase whose `state.db.last_built_commit_oid`
//! equals the current branch tip *and* whose `.pkg.tar.zst` is still on disk
//! is skipped, so re-running after declining the install just replays the
//! install step.

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
use std::fmt::Write as _;
use std::path::PathBuf;
use tracing::{debug, info, instrument, warn};

pub mod install;
pub mod makepkg;
pub mod review;
pub mod state_db;

use state_db::StateDb;

/// One built pkgbase's set of `.pkg.tar.zst` outputs.
struct BuiltPkg {
    pkgbase: String,
    files: Vec<PathBuf>,
}

/// Render the resolved [`Plan`] to stderr using the same grouped list format
/// the AUR confirmation prompt uses.
fn print_plan(plan: &Plan) {
    if plan.direct_repo.is_empty() && plan.transitive_repo.is_empty() && plan.aur_strata.is_empty()
    {
        ui::info("plan: nothing to do");
        return;
    }
    if !plan.direct_repo.is_empty() {
        ui::pkg_list("Repo packages (explicit)", &plan.direct_repo);
    }
    if !plan.transitive_repo.is_empty() {
        ui::pkg_list("Repo dependencies", &plan.transitive_repo);
    }
    if !plan.aur_strata.is_empty() {
        let total = plan.aur_strata.len();
        if total == 1 {
            ui::pkg_list("AUR build order", &plan.aur_strata[0]);
        } else {
            for (i, stratum) in plan.aur_strata.iter().enumerate() {
                ui::pkg_list(&format!("AUR build stratum {}/{total}", i + 1), stratum);
            }
        }
    }
}

/// Entry point for `gitaur -S <targets>`.
///
/// Loads the pacman snapshot and (optionally) the AUR index in parallel, then
/// classifies every target. After printing the unified plan and getting a
/// single confirmation gitaur drives every `pacman` call with `--noconfirm`
/// so the user is asked once; pacman never re-prompts. `already_confirmed`
/// short-circuits the gate for callers that have already confirmed at a
/// higher level (e.g. `-Syu` → [`cmd_sysupgrade`]); PKGBUILD review prompts
/// still respect `noconfirm`.
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

    print_plan(&plan);

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
        run_aur_pipeline(cfg, idx, &plan, noconfirm, asdeps)?;
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
    plan: &Plan,
    noconfirm: bool,
    asdeps: bool,
) -> Result<()> {
    let mirror = MirrorRepo::open(&paths::aur_repo_path())?;
    let mut db = StateDb::open(&paths::state_db_path())?;
    let direct_names: HashSet<&str> = plan
        .direct_targets
        .iter()
        .map(std::string::String::as_str)
        .collect();
    let mut transitive_marks: Vec<String> = Vec::new();

    for (stratum_idx, stratum) in plan.aur_strata.iter().enumerate() {
        if plan.aur_strata.len() > 1 {
            ui::info(&format!(
                "build stratum {}/{}: {}",
                stratum_idx + 1,
                plan.aur_strata.len(),
                stratum.join(" "),
            ));
        }
        let mut stratum_built: Vec<BuiltPkg> = Vec::with_capacity(stratum.len());
        for pkgbase in stratum {
            // Partial-split selection — present only for pkgbases where the
            // user asked for a subset. makepkg always packages every pkgname
            // in a split (no `--pkg=` flag); `build_one` returns only the
            // selected `.pkg.tar.zst` files so `install_stratum`'s
            // `pacman -U` transaction skips the rest.
            let selection = plan.pkgname_selections.get(pkgbase).map(Vec::as_slice);
            let outputs = build_one(cfg, &mirror, idx, &mut db, pkgbase, selection, noconfirm)?;
            stratum_built.push(BuiltPkg {
                pkgbase: pkgbase.clone(),
                files: outputs,
            });
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

#[instrument(skip(cfg, mirror, idx, db, selection))]
fn build_one(
    cfg: &Config,
    mirror: &MirrorRepo,
    idx: &IndexFile,
    db: &mut StateDb,
    pkgbase: &str,
    selection: Option<&[String]>,
    noconfirm: bool,
) -> Result<Vec<PathBuf>> {
    let entry = idx
        .entries
        .iter()
        .find(|e| e.pkgbase == pkgbase)
        .ok_or_else(|| Error::Build(format!("{pkgbase}: missing from index")))?;
    let head_hex = hex(&entry.commit_oid);
    let dest = paths::pkg_worktree(pkgbase);
    let wt = mirror::worktree::add_or_reset(mirror, pkgbase, &dest)?;

    // Idempotency: a cached build is reusable only when its on-disk
    // .pkg.tar.zst set covers every pkgname this run wants. A previous run
    // with `--pkg=A` produced just A; a follow-up that asks for {A,B} would
    // otherwise reuse A's file and silently drop B.
    if let Some(prev) = db.get(pkgbase)? {
        let existing = install::find_produced(&wt.path)?;
        let covers_selection = match selection {
            Some(sel) => sel.iter().all(|name| {
                existing
                    .iter()
                    .any(|f| install::extract_pkgname(f).as_deref() == Some(name.as_str()))
            }),
            None => entry.pkgnames.iter().all(|pkg| {
                existing
                    .iter()
                    .any(|f| install::extract_pkgname(f).as_deref() == Some(pkg.name.as_str()))
            }),
        };
        if prev.last_built_commit_oid == head_hex && covers_selection {
            let kept = filter_by_selection(&existing, selection);
            ui::note(&format!("{pkgbase}: already built at {}", &head_hex[..8]));
            debug!(
                pkgbase,
                head_hex,
                files = kept.len(),
                "reusing cached build"
            );
            return Ok(kept);
        }
    }

    review::review(db, mirror, pkgbase, &wt, noconfirm)?;
    ui::step(&format!("makepkg {pkgbase}"));
    makepkg::run(cfg, &wt.path)?;

    let produced = install::find_produced(&wt.path)?;
    let outputs = filter_by_selection(&produced, selection);
    if outputs.is_empty() {
        return Err(Error::Build(format!(
            "{pkgbase}: makepkg produced no packages"
        )));
    }

    let version = version_string(entry);
    db.record_build(pkgbase, &head_hex, &version)?;
    info!(pkgbase, version, files = outputs.len(), "build recorded");
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
///
// TODO: match paru/yay's `--devel`: cache the upstream source commit OID per
// pkgbase in state.db and only mark a VCS pkg outdated when `git ls-remote`
// reports a different ref. Today every `--devel` run schedules a rebuild for
// every VCS pkgbase, which is correct but wasteful.
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
        let aur_ver = version_string(entry);
        let need = (devel && is_vcs) || vercmp::is_outdated(&installed_ver, &aur_ver);
        if need {
            out.push(PkgUpgrade {
                name,
                old_ver: installed_ver,
                new_ver: aur_ver,
            });
        }
    }
    out
}

/// `gitaur -Qu` — show the union of pacman-repo and AUR upgrade candidates
/// as two aligned, colorized tables grouped by source. Read-only and
/// unprivileged (no sudo), so safe to call both as the bare `-Qu` and as
/// a preview before `-Syu` runs.
#[instrument]
pub fn cmd_query_upgrades(devel: bool) -> Result<u8> {
    let mut repo = invoke::query_repo_upgrades()?;
    let alpm = alpm_db::open()?;
    let pac = PacmanIndex::build(&alpm);
    let idx_path = paths::index_path();
    let mut aur = if idx_path.exists() {
        let idx = index::load(&idx_path)?;
        let by = Secondary::build(&idx);
        aur_upgrades(&idx, &by, &pac, devel)
    } else {
        Vec::new()
    };
    repo.sort_by(|a, b| a.name.cmp(&b.name));
    aur.sort_by(|a, b| a.name.cmp(&b.name));
    ui::upgrade_table("Repo upgrades", &repo);
    ui::upgrade_table("AUR upgrades", &aur);
    Ok(0)
}

/// Entry point for the AUR half of `-Syu`. The dispatch-level `-Syu` confirm
/// has already accepted the unified plan, so `already_confirmed=true` short-
/// circuits `cmd_install`'s own gate (PKGBUILD review still honors
/// `noconfirm`).
pub fn cmd_sysupgrade(cfg: &Config, devel: bool, noconfirm: bool) -> Result<u8> {
    let idx = index::load(&paths::index_path())?;
    let by = Secondary::build(&idx);
    let alpm = alpm_db::open()?;
    let pac = PacmanIndex::build(&alpm);
    let queue: Vec<String> = aur_upgrades(&idx, &by, &pac, devel)
        .into_iter()
        .map(|u| u.name)
        .collect();
    if queue.is_empty() {
        ui::info("nothing to do");
        return Ok(0);
    }
    cmd_install(cfg, &queue, noconfirm, false, true)
}

/// Entry point for `-Sc` / `-Scc`.
#[instrument(skip(cfg, argv))]
pub fn cmd_clean(cfg: &Config, deep: bool, argv: &[String]) -> Result<u8> {
    invoke::exec_pacman(cfg, argv)?;

    let pkgs_root = paths::state_dir().join("pkgs");
    if pkgs_root.exists() {
        ui::info("removing per-pkgbase worktrees");
        if let Err(e) = std::fs::remove_dir_all(&pkgs_root) {
            warn!(error = %e, "could not remove pkgs dir");
        }
        let _ = std::fs::create_dir_all(&pkgs_root);
    }
    if deep {
        ui::info("clearing build state DB");
        let db_path = paths::state_db_path();
        if db_path.exists() {
            std::fs::remove_file(&db_path)?;
        }
    }
    Ok(0)
}

fn is_vcs_pkg(pkgbase: &str) -> bool {
    pkgbase.ends_with("-git")
        || pkgbase.ends_with("-svn")
        || pkgbase.ends_with("-hg")
        || pkgbase.ends_with("-bzr")
}

fn version_string(e: &IndexEntry) -> String {
    let epoch = e
        .epoch
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!("{s}:"))
        .unwrap_or_default();
    format!("{epoch}{}-{}", e.pkgver, e.pkgrel)
}

fn hex(b: &[u8; 20]) -> String {
    let mut s = String::with_capacity(40);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
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

    #[test]
    fn version_with_epoch() {
        let e = IndexEntry {
            pkgver: "1.0".into(),
            pkgrel: "2".into(),
            epoch: Some("3".into()),
            ..Default::default()
        };
        assert_eq!(version_string(&e), "3:1.0-2");
    }

    #[test]
    fn version_without_epoch() {
        let e = IndexEntry {
            pkgver: "1.0".into(),
            pkgrel: "2".into(),
            ..Default::default()
        };
        assert_eq!(version_string(&e), "1.0-2");
    }

    #[test]
    fn hex_encodes() {
        let mut b = [0u8; 20];
        b[0] = 0xde;
        b[1] = 0xad;
        assert!(hex(&b).starts_with("dead"));
    }
}
