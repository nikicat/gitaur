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

/// Entry point for `gitaur -S <targets>`.
///
/// Loads the pacman snapshot and (optionally) the AUR index in parallel, then
/// classifies every target. Pure-repo plans hand off straight to `pacman -S`
/// so the user sees pacman's native UI; only mixed/AUR plans run the full
/// build pipeline.
#[instrument(skip(cfg))]
pub fn cmd_install(cfg: &Config, targets: &[String], noconfirm: bool, asdeps: bool) -> Result<u8> {
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

    let plan = resolver::resolve(cfg, idx, by, &pac, targets)?;

    // Pure-repo fast path: nothing to build, delegate to pacman so the user
    // gets pacman's own "Proceed with installation?" prompt verbatim. Direct
    // targets stay explicit; transitive repo deps (none here, since AUR is
    // empty) would be marked --asdeps via a follow-up `pacman -D`.
    if plan.aur_order.is_empty() {
        if plan.direct_repo.is_empty() && plan.transitive_repo.is_empty() {
            ui::info("nothing to do");
            return Ok(0);
        }
        let mut args = vec!["-S".to_string(), "--needed".into()];
        if noconfirm {
            args.push("--noconfirm".into());
        }
        if asdeps {
            args.push("--asdeps".into());
        }
        args.extend(plan.direct_repo.iter().cloned());
        args.extend(plan.transitive_repo.iter().cloned());
        return invoke::exec_pacman(cfg, &args);
    }

    // AUR path needs a loaded index — by construction `aur_order` is empty
    // when `by == None`, so this unwrap is unreachable.
    let idx = aur_loaded
        .as_ref()
        .map(|(i, _)| i)
        .ok_or_else(|| Error::other("internal: AUR plan without index"))?;

    if !plan.direct_repo.is_empty() {
        ui::pkg_list("Repo packages (explicit)", &plan.direct_repo);
    }
    if !plan.transitive_repo.is_empty() {
        ui::pkg_list("Repo dependencies", &plan.transitive_repo);
    }
    ui::pkg_list("AUR build order", &plan.aur_order);

    if !ui::confirm("Proceed with build?", noconfirm)? {
        return Err(Error::UserAbort);
    }

    // Phase 1 — repo deps. Two batches so direct targets stay explicit and
    // transitive deps get --asdeps. Sudo cache typically bridges the gap.
    if !plan.direct_repo.is_empty() {
        ui::info("installing repo packages");
        let mut args = vec!["-S".to_string(), "--needed".into()];
        if noconfirm {
            args.push("--noconfirm".into());
        }
        args.extend(plan.direct_repo.iter().cloned());
        invoke::exec_pacman(cfg, &args)?;
    }
    if !plan.transitive_repo.is_empty() {
        ui::info("installing repo dependencies");
        let mut args = vec!["-S".to_string(), "--needed".into(), "--asdeps".into()];
        if noconfirm {
            args.push("--noconfirm".into());
        }
        args.extend(plan.transitive_repo.iter().cloned());
        invoke::exec_pacman(cfg, &args)?;
    }

    // Phase 2 — unprivileged build loop. No sudo, no keepalive.
    let mirror = MirrorRepo::open(&paths::aur_repo_path())?;
    let mut db = StateDb::open(&paths::state_db_path())?;
    let mut built: Vec<BuiltPkg> = Vec::with_capacity(plan.aur_order.len());
    for pkgbase in &plan.aur_order {
        let outputs = build_one(cfg, &mirror, idx, &mut db, pkgbase, noconfirm)?;
        built.push(BuiltPkg {
            pkgbase: pkgbase.clone(),
            files: outputs,
        });
    }

    // Phase 3 — batched pacman -U at the very end (single sudo prompt; user may decline).
    install_all(cfg, idx, &plan, &built, noconfirm, asdeps)?;
    Ok(0)
}

#[instrument(skip(cfg, mirror, idx, db))]
fn build_one(
    cfg: &Config,
    mirror: &MirrorRepo,
    idx: &IndexFile,
    db: &mut StateDb,
    pkgbase: &str,
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

    // Idempotency: skip the build if we already produced .pkg.tar.zst at this commit.
    if let Some(prev) = db.get(pkgbase)? {
        let existing = install::find_produced(&wt.path)?;
        if prev.last_built_commit_oid == head_hex && !existing.is_empty() {
            ui::note(&format!("{pkgbase}: already built at {}", &head_hex[..8]));
            debug!(
                pkgbase,
                head_hex,
                files = existing.len(),
                "reusing cached build"
            );
            return Ok(existing);
        }
    }

    review::review(db, mirror, pkgbase, &wt, noconfirm)?;
    ui::step(&format!("makepkg {pkgbase}"));
    makepkg::run(cfg, &wt.path)?;

    let outputs = install::find_produced(&wt.path)?;
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

#[instrument(skip(cfg, idx, plan, built))]
fn install_all(
    cfg: &Config,
    idx: &IndexFile,
    plan: &Plan,
    built: &[BuiltPkg],
    noconfirm: bool,
    asdeps_override: bool,
) -> Result<()> {
    if built.is_empty() {
        return Ok(());
    }
    let total: usize = built.iter().map(|b| b.files.len()).sum();
    ui::step(&format!("installing {total} built package(s) with pacman"));
    if !ui::confirm("Install built packages now?", noconfirm)? {
        ui::note("install declined; rerun `gitaur -S …` to replay this step");
        return Err(Error::UserAbort);
    }

    let direct: HashSet<&str> = plan
        .direct_targets
        .iter()
        .map(std::string::String::as_str)
        .collect();
    let mut direct_files: Vec<PathBuf> = Vec::new();
    let mut transitive_files: Vec<PathBuf> = Vec::new();

    for b in built {
        let entry = idx
            .entries
            .iter()
            .find(|e| e.pkgbase == b.pkgbase)
            .ok_or_else(|| Error::Build(format!("{}: missing from index", b.pkgbase)))?;
        partition_pkgs(
            entry,
            &b.files,
            &direct,
            asdeps_override,
            &mut direct_files,
            &mut transitive_files,
        );
    }

    if !direct_files.is_empty() {
        let mut args = vec!["-U".to_string(), "--needed".into()];
        if noconfirm {
            args.push("--noconfirm".into());
        }
        for p in &direct_files {
            args.push(p.to_string_lossy().into_owned());
        }
        invoke::exec_pacman(cfg, &args)?;
    }
    if !transitive_files.is_empty() {
        let mut args = vec!["-U".to_string(), "--needed".into(), "--asdeps".into()];
        if noconfirm {
            args.push("--noconfirm".into());
        }
        for p in &transitive_files {
            args.push(p.to_string_lossy().into_owned());
        }
        invoke::exec_pacman(cfg, &args)?;
    }
    Ok(())
}

/// Partition `files` into (direct, transitive) using the entry's pkgnames + plan targets.
fn partition_pkgs(
    entry: &IndexEntry,
    files: &[PathBuf],
    direct: &HashSet<&str>,
    asdeps_override: bool,
    direct_out: &mut Vec<PathBuf>,
    transitive_out: &mut Vec<PathBuf>,
) {
    for f in files {
        let pkgname = install::extract_pkgname(f).unwrap_or_default();
        let is_direct = !asdeps_override
            && (direct.contains(pkgname.as_str())
                || entry
                    .pkgnames
                    .iter()
                    .any(|n| n == &pkgname && direct.contains(n.as_str())));
        if is_direct {
            direct_out.push(f.clone());
        } else {
            transitive_out.push(f.clone());
        }
    }
}

/// Entry point for the AUR half of `-Syu`.
pub fn cmd_sysupgrade(cfg: &Config, devel: bool, noconfirm: bool) -> Result<u8> {
    let idx = index::load(&paths::index_path())?;
    let by = Secondary::build(&idx);
    let alpm = alpm_db::open()?;
    let pac = PacmanIndex::build(&alpm);
    let foreign = pac.foreign();

    let mut queue: Vec<String> = Vec::new();
    for (name, installed_ver) in foreign {
        let Some(entry) = by.lookup(&idx, &name) else {
            warn!(name, "foreign pkg not in AUR index");
            continue;
        };
        if !devel && is_vcs_pkg(&entry.pkgbase) {
            continue;
        }
        let aur_ver = version_string(entry);
        let need = if devel && is_vcs_pkg(&entry.pkgbase) {
            true
        } else {
            vercmp::is_outdated(&installed_ver, &aur_ver)
        };
        if need {
            queue.push(name);
        }
    }
    if queue.is_empty() {
        ui::info("nothing to do");
        return Ok(0);
    }
    ui::pkg_list("AUR upgrades", &queue);
    cmd_install(cfg, &queue, noconfirm, false)
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
