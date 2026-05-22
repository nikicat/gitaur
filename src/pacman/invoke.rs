//! Spawn `pacman` (with sudo gating) for pass-through and `-U` installs.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::names::PkgName;
use crate::pacman::alpm_db;
use crate::version::Version;
use std::process::Command;
use tracing::{debug, info, instrument};

/// Sentinel value [`PkgUpgrade::repo`] carries for AUR-sourced rows.
pub const REPO_AUR: &str = "aur";

/// One package whose installed version is older than what's available
/// in a sync repo or in the AUR index.
///
/// `repo` is the pacman sync-DB name (`core`, `extra`, `multilib`, …) for
/// repo upgrades, or [`REPO_AUR`] for AUR upgrades. It drives both grouping
/// in the upgrade table and the source column shown to the user.
#[derive(Debug, Clone, PartialEq)]
pub struct PkgUpgrade {
    pub repo: String,
    pub name: PkgName,
    pub old_ver: Version,
    pub new_ver: Version,
}

/// Walk alpm directly for upgradable repo packages — no shell-out, no parser.
///
/// For every installed package, the first sync DB (in pacman.conf order) that
/// declares the same pkgname wins; if its version is newer we record an
/// upgrade and tag it with that DB's name. Packages absent from every sync DB
/// (foreign / AUR) are skipped here — they go through [`crate::build`].
#[instrument]
pub fn query_repo_upgrades() -> Result<Vec<PkgUpgrade>> {
    let alpm = alpm_db::open()?;
    let mut upgrades = Vec::new();
    for ipkg in alpm.localdb().pkgs() {
        for db in alpm.syncdbs() {
            let Ok(spkg) = db.pkg(ipkg.name()) else {
                continue;
            };
            // `ipkg.version()` / `spkg.version()` return `&alpm::Ver`; the
            // `From<&alpm::Ver> for Version` impl reads the bytes directly
            // via `Ver::as_str()` — no `Display`/`to_string()` round trip.
            let installed = Version::from(ipkg.version());
            let avail = Version::from(spkg.version());
            if installed.is_outdated(&avail) {
                upgrades.push(PkgUpgrade {
                    repo: db.name().to_string(),
                    name: PkgName::new(ipkg.name()),
                    old_ver: installed,
                    new_ver: avail,
                });
            }
            // First syncdb that declares this pkgname is the one pacman would
            // pull from — don't keep scanning later DBs even if they also
            // carry it (e.g. testing repos shadowing core).
            break;
        }
    }
    debug!(count = upgrades.len(), "alpm repo upgrades scanned");
    Ok(upgrades)
}

/// Exec `pacman` with `argv`, prepending the privilege escalator when needed.
#[instrument(skip(cfg))]
pub fn exec_pacman(cfg: &Config, argv: &[String]) -> Result<u8> {
    let escalate = needs_sudo(argv);
    let (program, args) = if escalate {
        (cfg.privilege_escalator.as_str(), with_pacman(argv))
    } else {
        ("pacman", argv.to_vec())
    };
    debug!(program, args = ?args, "spawning pacman");
    let status = Command::new(program).args(&args).status()?;
    let code = status.code().unwrap_or(1);
    info!(code, "pacman exited");
    if status.success() {
        Ok(0)
    } else {
        Err(Error::PacmanExit(code))
    }
}

fn with_pacman(argv: &[String]) -> Vec<String> {
    let mut v = vec!["pacman".to_string()];
    v.extend_from_slice(argv);
    v
}

/// Heuristic: operations that mutate the system require sudo unless `--print`.
fn needs_sudo(argv: &[String]) -> bool {
    let has = |s: &str| argv.iter().any(|a| a == s);
    if has("--print") || has("-p") {
        return false;
    }
    argv.iter().any(|a| {
        matches!(
            a.as_str(),
            "-S" | "-Sy" | "-Syu" | "-Syyu" | "-R" | "-Rs" | "-Rns" | "-U" | "-Sc" | "-Scc" | "-D"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sudo_for_mutating_ops() {
        assert!(needs_sudo(&["-Syu".into()]));
        assert!(needs_sudo(&["-Rns".into(), "foo".into()]));
        assert!(needs_sudo(&["-U".into(), "x.pkg.tar.zst".into()]));
    }

    #[test]
    fn no_sudo_for_queries() {
        assert!(!needs_sudo(&["-Qi".into(), "vim".into()]));
        assert!(!needs_sudo(&["-Ss".into(), "vim".into()]));
    }

    #[test]
    fn print_flag_disables_sudo() {
        assert!(!needs_sudo(&["-Syu".into(), "--print".into()]));
        assert!(!needs_sudo(&["-Syu".into(), "-p".into()]));
    }
}
