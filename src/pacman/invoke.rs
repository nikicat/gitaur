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
///
/// Recognises arbitrary `-S` clusters rather than an exact-string allowlist —
/// `-Su`, `-Syy`, `-Syuw`, etc. all reach pacman from gitaur's dispatch and
/// every variant mutates either the local DB or the cache. The carve-out is
/// `-Ss` (search) and `-Si` (info), which are pure queries; in practice those
/// are routed to gitaur's own handlers before reaching this function, but the
/// guard keeps the classifier independently correct.
fn needs_sudo(argv: &[String]) -> bool {
    let has = |s: &str| argv.iter().any(|a| a == s);
    if has("--print") || has("-p") {
        return false;
    }
    let Some(op) = argv
        .iter()
        .find(|a| a.starts_with('-') && !a.starts_with("--"))
    else {
        return false;
    };
    let op = op.as_str();
    if op.starts_with("-R") || op == "-U" || op == "-D" {
        return true;
    }
    if let Some(rest) = op.strip_prefix("-S") {
        // `-Ss` / `-Si` are queries; every other `-S` variant (`-S` install,
        // `-Sy` refresh, `-Su` upgrade, `-Sc` clean, `-Sw` download, …)
        // mutates root-owned state.
        return !rest.contains(['s', 'i']);
    }
    false
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
    fn sudo_for_all_sync_cluster_variants() {
        // Every cluster pacman accepts as a state-changing -S op must be
        // recognised — the old allowlist missed -Su entirely and any
        // `-Syyy*` future variant.
        for op in [
            "-S", "-Su", "-Sy", "-Syy", "-Syu", "-Syyu", "-Suy", "-Sw", "-Swy", "-Sc", "-Scc",
        ] {
            assert!(needs_sudo(&[op.into()]), "expected sudo for {op}");
        }
    }

    #[test]
    fn no_sudo_for_search_or_info_modifiers() {
        // -Ss / -Si remain queries even when clustered with refresh letters,
        // matching pacman's semantics (`pacman -Sys` is still search-shaped).
        for op in ["-Ss", "-Si", "-Sii", "-Sys", "-Syi"] {
            assert!(!needs_sudo(&[op.into()]), "expected no sudo for {op}");
        }
    }

    #[test]
    fn sudo_for_remove_variants() {
        for op in ["-R", "-Rs", "-Rns", "-Rnsc", "-Rdd"] {
            assert!(needs_sudo(&[op.into()]), "expected sudo for {op}");
        }
    }

    #[test]
    fn no_sudo_when_no_op_present() {
        // Bare argv (positional only, or empty) shouldn't escalate.
        assert!(!needs_sudo(&[]));
        assert!(!needs_sudo(&["vim".into()]));
    }

    #[test]
    fn long_flags_do_not_count_as_op() {
        // `--noconfirm` alone isn't an op; without a short-flag op the call
        // is a pacman misuse but we mustn't preemptively escalate.
        assert!(!needs_sudo(&["--noconfirm".into()]));
        // …but if a real op is also present, it wins.
        assert!(needs_sudo(&["--noconfirm".into(), "-Su".into()]));
    }

    #[test]
    fn print_flag_disables_sudo_for_su_too() {
        // Regression: -Su was the case the allowlist missed; -Su --print
        // must still be classified as no-sudo.
        assert!(!needs_sudo(&["-Su".into(), "--print".into()]));
        assert!(!needs_sudo(&["-Su".into(), "-p".into()]));
    }
}
