//! Spawn `pacman` (with sudo gating) for pass-through and `-U` installs.

use crate::config::Config;
use crate::error::{Error, Result};
use std::process::Command;
use tracing::{debug, info, instrument};

/// One package whose installed version is older than what's available
/// in a sync repo (a row of `pacman -Qu` output) or in the AUR index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkgUpgrade {
    pub name: String,
    pub old_ver: String,
    pub new_ver: String,
}

/// Query `pacman -Qu` for upgradable repo packages. Runs unprivileged: the
/// local + sync DBs are world-readable, so the plan can be shown before any
/// sudo prompt. Exit status 1 with empty stdout is pacman's "nothing to
/// upgrade" — treated as `Ok(vec![])`, not an error.
#[instrument]
pub fn query_repo_upgrades() -> Result<Vec<PkgUpgrade>> {
    let out = Command::new("pacman").arg("-Qu").output()?;
    let code = out.status.code().unwrap_or(-1);
    if code == 1 && out.stdout.is_empty() {
        return Ok(Vec::new());
    }
    if !out.status.success() {
        return Err(Error::other(format!(
            "pacman -Qu exited with status {code}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut upgrades = Vec::new();
    for line in text.lines() {
        if let Some(u) = parse_qu_line(line) {
            upgrades.push(u);
        }
    }
    debug!(count = upgrades.len(), "pacman -Qu parsed");
    Ok(upgrades)
}

/// Parse one line of `pacman -Qu` output: `name old_ver -> new_ver`. Anything
/// else (blank lines, `[ignored]` markers pacman tacks onto held packages)
/// returns `None` so the caller drops it silently.
fn parse_qu_line(line: &str) -> Option<PkgUpgrade> {
    let mut parts = line.split_whitespace();
    let name = parts.next()?;
    let old_ver = parts.next()?;
    if parts.next()? != "->" {
        return None;
    }
    let new_ver = parts.next()?;
    Some(PkgUpgrade {
        name: name.to_string(),
        old_ver: old_ver.to_string(),
        new_ver: new_ver.to_string(),
    })
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

    #[test]
    fn parses_qu_line() {
        assert_eq!(
            parse_qu_line("vim 9.0-1 -> 9.1-2"),
            Some(PkgUpgrade {
                name: "vim".into(),
                old_ver: "9.0-1".into(),
                new_ver: "9.1-2".into(),
            })
        );
    }

    #[test]
    fn rejects_malformed_qu_lines() {
        assert_eq!(parse_qu_line(""), None);
        assert_eq!(parse_qu_line("vim 9.0-1"), None);
        assert_eq!(parse_qu_line("vim 9.0-1 => 9.1-2"), None);
    }
}
