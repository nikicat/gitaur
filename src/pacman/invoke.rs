//! Spawn `pacman` (with sudo gating) for pass-through and `-U` installs.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::names::PkgName;
use crate::pacman::alpm_db;
use crate::runopts;
use crate::ui;
use crate::version::Version;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;
use tracing::{debug, info, instrument, warn};

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
                    repo: db.name().to_owned(),
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
///
/// When escalation kicks in, the user sees the exact command first and gates
/// it with a y/n confirm. Without this, the escalator (typically `sudo`)
/// pops a password prompt with no context about what gitaur is about to
/// run — a hostile UX especially mid-build when several minutes have
/// elapsed since the user's last interaction. The `noconfirm` flag (read
/// from [`runopts`]) suppresses the prompt for non-interactive callers.
#[instrument(skip(cfg))]
pub fn exec_pacman(cfg: &Config, argv: &[String]) -> Result<u8> {
    let escalate = needs_sudo(argv);
    let (program, spawn_args) = if escalate {
        (cfg.privilege_escalator.as_str(), with_pacman(argv))
    } else {
        ("pacman", argv.to_vec())
    };
    if escalate {
        confirm_escalation(program, &spawn_args)?;
    }
    debug!(program, args = ?spawn_args, "spawning pacman");
    let status = Command::new(program).args(&spawn_args).status()?;
    let code = status_to_exit_code(status);
    if status.success() {
        Ok(0)
    } else {
        Err(Error::PacmanExit(code))
    }
}

/// Collapse a child [`std::process::ExitStatus`] to a single `i32` the caller
/// can propagate. `status.code()` returns `None` iff the child was killed by
/// a signal (OOM, SIGTERM, ^C) — picking up [`ExitStatusExt::signal`] keeps
/// that distinguishable from a normal `pacman` exit 1 in both logs and the
/// bubbled-up `Error::PacmanExit`. POSIX shells use `128 + signal` for the
/// propagated code (bash reports 137 for SIGKILL, 143 for SIGTERM), so we do
/// the same.
fn status_to_exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(c) = status.code() {
        info!(code = c, "pacman exited");
        c
    } else {
        let sig = status.signal().unwrap_or(0);
        warn!(signal = sig, "pacman was killed by signal");
        128 + sig
    }
}

/// Show what's about to run with elevated privileges and gate it with a
/// y/n confirm. No-op under `--noconfirm` (returns `Ok(())` immediately).
///
/// Rings the terminal bell before the prompt: a `pacman -U` after a long
/// AUR build can fire 5–30 min after the user's last interaction, and the
/// bell pulls their attention back. Skipped under `--noconfirm` (no one
/// is waiting) and inside [`ui::bell`] when stderr isn't a TTY.
fn confirm_escalation(program: &str, spawn_args: &[String]) -> Result<()> {
    let noconfirm = runopts::noconfirm();
    if !noconfirm {
        ui::bell();
    }
    ui::info(&format!(
        "about to elevate via {program}:\n   {program} {}",
        spawn_args.join(" "),
    ));
    if !ui::confirm("Continue?", noconfirm)? {
        return Err(Error::UserAbort);
    }
    Ok(())
}

fn with_pacman(argv: &[String]) -> Vec<String> {
    let mut v = vec!["pacman".to_owned()];
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

    // Linux wait-status encoding (per waitpid(2)): low 7 bits = signal,
    // bit 7 = core-dump flag, bits 8-15 = exit code. Signal 0 means "exited
    // normally" so `from_raw(N << 8)` synthesises a clean exit-code-N status,
    // and `from_raw(SIG)` synthesises a signal-N kill.

    #[test]
    fn exit_code_zero_propagates() {
        let s = std::process::ExitStatus::from_raw(0);
        assert_eq!(status_to_exit_code(s), 0);
    }

    #[test]
    fn exit_code_nonzero_propagates() {
        let s = std::process::ExitStatus::from_raw(1 << 8);
        assert_eq!(status_to_exit_code(s), 1);
    }

    #[test]
    fn sigkill_maps_to_137() {
        let s = std::process::ExitStatus::from_raw(9);
        assert!(s.code().is_none(), "sanity: SIGKILL has no exit code");
        assert_eq!(s.signal(), Some(9));
        assert_eq!(status_to_exit_code(s), 137);
    }

    #[test]
    fn sigterm_maps_to_143() {
        let s = std::process::ExitStatus::from_raw(15);
        assert_eq!(status_to_exit_code(s), 143);
    }
}
