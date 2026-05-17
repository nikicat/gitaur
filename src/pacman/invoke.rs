//! Spawn `pacman` (with sudo gating) for pass-through and `-U` installs.

use crate::config::Config;
use crate::error::{Error, Result};
use std::process::Command;
use tracing::{debug, info, instrument};

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
