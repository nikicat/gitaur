//! Spawn `pacman` (with sudo gating) for pass-through and `-U` installs.

use crate::config::Config;
use crate::context;
use crate::error::{Error, Result};
use crate::names::{PkgName, RepoName};
use crate::pacman::alpm_db;
use crate::runopts;
use crate::ui;
use crate::version::Version;
use alpm::{Alpm, PrepareData, PrepareError, SigLevel, TransFlag};
use console::strip_ansi_codes;
use std::io::{IsTerminal, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use tracing::{debug, error, info, instrument, warn};

/// The sentinel value [`PkgUpgrade::repo`] carries for AUR-sourced rows.
///
/// Kept as a `&str` (not a [`RepoName`]) so it doubles as a `match` pattern and
/// a `RepoName == REPO_AUR` comparison target — `RepoName: PartialEq<&str>`.
pub const REPO_AUR: &str = "aur";

/// One package whose installed version is older than what's available
/// in a sync repo or in the AUR index.
///
/// `repo` is the pacman sync-DB name (`core`, `extra`, `multilib`, …) for
/// repo upgrades, or [`REPO_AUR`] for AUR upgrades. It drives both grouping
/// in the upgrade table and the source column shown to the user.
#[derive(Debug, Clone, PartialEq)]
pub struct PkgUpgrade {
    pub repo: RepoName,
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
    // Read available versions from gitaur's rootless sync db when it's been
    // refreshed (`-Sy`), so the upgrade list reflects the latest repos without
    // a privileged `pacman -Sy`. Falls back to the system db otherwise.
    let alpm = alpm_db::open_synced()?;
    Ok(query_repo_upgrades_in(&alpm))
}

/// Like [`query_repo_upgrades`] but against a caller-supplied handle.
///
/// Lets the upgrade loop build the repo and AUR halves of one recompute from a
/// single localdb snapshot instead of opening alpm twice per iteration.
#[instrument(skip(alpm))]
pub fn query_repo_upgrades_in(alpm: &Alpm) -> Vec<PkgUpgrade> {
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
                    repo: RepoName::from(db.name()),
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
    upgrades
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
    // For `-U <files>`, ask libalpm what would happen before pacman runs.
    // Pacman under `--noconfirm` suppresses the conflict-pair detail (it
    // would normally be the body of an interactive prompt), so the only
    // way to get structured diagnostics into the execution log is to do
    // the prepare ourselves. Best-effort: any failure is logged at debug
    // and we still hand off to the real pacman.
    preflight_dash_u(argv);

    // On a real terminal, hand pacman the inherited TTY: it draws its own
    // download + transaction progress bars and reads prompts (and the sudo
    // password) natively. Capturing output means piping pacman's stdout, which
    // forces its degraded line-by-line mode — and can kill pacman with "unable
    // to write to pipe" if our reader closes first — so we only tee off a
    // terminal (cron / CI / a pipe), where there are no bars to lose and the
    // execution log still wants pacman's output on failure (the contract
    // `tests/container/smoke/57_pacman_conflict_logged` pins).
    if std::io::stdout().is_terminal() {
        exec_pacman_inherited(program, &spawn_args)
    } else {
        exec_pacman_teed(program, &spawn_args)
    }
}

/// Run pacman attached to the inherited stdio — its own progress bars and native
/// prompts, no output capture (the user is watching live). Maps the exit status
/// to gitaur's `Ok(0)` / [`Error::PacmanExit`].
fn exec_pacman_inherited(program: &str, spawn_args: &[String]) -> Result<u8> {
    let status = Command::new(program).args(spawn_args).status()?;
    let code = status_to_exit_code(status);
    if status.success() {
        Ok(0)
    } else {
        Err(Error::PacmanExit(code))
    }
}

/// Run pacman with stdout+stderr teed to ours *and* an in-memory copy, so a
/// failure can be reconstructed from the execution log without scrollback. Both
/// channels matter — pacman prints the "X and Y are in conflict" pair on stdout
/// (a prompt body), while the terminal "error: ..." lines go to stderr. Used off
/// a terminal, where pacman has no progress UI to lose to the pipe anyway.
fn exec_pacman_teed(program: &str, spawn_args: &[String]) -> Result<u8> {
    let mut child = Command::new(program)
        .args(spawn_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out_pipe = child.stdout.take().expect("stdout piped above");
    let err_pipe = child.stderr.take().expect("stderr piped above");
    let tee_out = context::spawn(move || tee_pipe(out_pipe, std::io::stdout()));
    let tee_err = context::spawn(move || tee_pipe(err_pipe, std::io::stderr()));
    let status = child.wait()?;
    // A panicked tee thread shouldn't mask pacman's exit; treat it as a
    // lost-capture and keep going so the caller still gets the code.
    let captured_out = tee_out.join().unwrap_or_else(|_| {
        warn!("pacman stdout tee thread panicked; captured output lost");
        String::new()
    });
    let captured_err = tee_err.join().unwrap_or_else(|_| {
        warn!("pacman stderr tee thread panicked; captured output lost");
        String::new()
    });
    let code = status_to_exit_code(status);
    if status.success() {
        Ok(0)
    } else {
        log_pacman_output_on_failure(&captured_out, &captured_err);
        Err(Error::PacmanExit(code))
    }
}

/// Emit a single structured error event with pacman's full captured output.
/// ANSI escapes are stripped so the log line is readable in a plain viewer.
fn log_pacman_output_on_failure(captured_out: &str, captured_err: &str) {
    if let Some(c) = clean_pacman_capture(captured_out, captured_err) {
        error!(
            stdout = %c.stdout,
            stderr = %c.stderr,
            "pacman output captured on failure",
        );
    }
}

/// stdout/stderr from a failed pacman run, cleaned for logging.
struct CleanedPacmanCapture<'a> {
    stdout: std::borrow::Cow<'a, str>,
    stderr: std::borrow::Cow<'a, str>,
}

/// Strip ANSI escapes + trailing newlines from each channel. Returns `None`
/// when both channels are empty after cleaning — caller skips the log event
/// in that case so we don't emit `stdout="" stderr=""` noise.
fn clean_pacman_capture<'a>(
    captured_out: &'a str,
    captured_err: &'a str,
) -> Option<CleanedPacmanCapture<'a>> {
    let stdout = strip_ansi_codes(captured_out.trim_end_matches('\n'));
    let stderr = strip_ansi_codes(captured_err.trim_end_matches('\n'));
    if stdout.is_empty() && stderr.is_empty() {
        return None;
    }
    Some(CleanedPacmanCapture { stdout, stderr })
}

/// Forward bytes from `src` to `sink` and accumulate them into a String for
/// later logging. The sink is the user's real stdout/stderr in production;
/// the returned buffer goes into the execution log when pacman exits non-zero.
fn tee_pipe<R: Read, W: Write>(mut src: R, mut sink: W) -> String {
    let mut buf = String::new();
    let mut chunk = [0u8; 4096];
    while let Ok(n) = src.read(&mut chunk) {
        if n == 0 {
            break;
        }
        let bytes = &chunk[..n];
        sink.write_all(bytes).ok();
        sink.flush().ok();
        buf.push_str(&String::from_utf8_lossy(bytes));
    }
    buf
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

/// Run a read-only libalpm `trans_prepare` against `-U` artifact paths to
/// surface the conflict / unsatisfied-dep set BEFORE spawning pacman.
///
/// Pacman with `--noconfirm` swallows the offending pair from its terminal
/// output (it would have been the body of the interactive "Replace X with
/// Y?" prompt). The execution log then only sees the generic
/// `unresolvable package conflicts detected` line. Computing the prepare
/// ourselves writes the structured pair (`pkg1`, `pkg2`, `reason`) into
/// the log as tracing fields, so the next failure is diagnosable from the
/// log alone.
///
/// Best-effort: any failure here is logged at debug and the caller proceeds
/// with the real pacman invocation. We never block an install on the
/// pre-flight succeeding (alpm might refuse for sig/db/lock reasons that
/// don't reflect the real install path).
fn preflight_dash_u(argv: &[String]) {
    let Some(paths) = dash_u_paths(argv) else {
        return;
    };
    if paths.is_empty() {
        return;
    }
    match preflight_dash_u_inner(&paths) {
        Ok(PreflightOutcome::Flagged) => debug!(
            count = paths.len(),
            "pacman preflight: prepare flagged issues (see warnings above)",
        ),
        Ok(PreflightOutcome::Clean) => {
            debug!(count = paths.len(), "pacman preflight: prepare clean");
        }
        Err(e) => debug!(error = %e, "pacman preflight skipped (could not run prepare)"),
    }
}

/// What `trans_prepare` told us about the artifact set.
///
/// `Flagged` means alpm returned a `PrepareError` whose detail (conflict
/// pair / unsatisfied dep / invalid arch) we've already emitted as structured
/// `warn!` events — the variant just records that the diagnostic fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreflightOutcome {
    Clean,
    Flagged,
}

/// Extract the file arguments to a `-U` invocation, ignoring flags. Returns
/// `None` when `argv` isn't a `-U` at all (so callers skip the pre-flight).
fn dash_u_paths(argv: &[String]) -> Option<Vec<&str>> {
    let mut found_u = false;
    let mut paths = Vec::new();
    for a in argv {
        if a == "-U" {
            found_u = true;
            continue;
        }
        if !found_u {
            continue;
        }
        if a.starts_with('-') {
            continue;
        }
        paths.push(a.as_str());
    }
    found_u.then_some(paths)
}

/// Run the read-only prepare against `paths` and return a [`PreflightOutcome`]
/// describing whether issues were reported (and already logged). `Err` means
/// we couldn't even run the prepare — caller treats that as best-effort skip.
fn preflight_dash_u_inner(paths: &[&str]) -> Result<PreflightOutcome> {
    let mut alpm = alpm_db::open()?;
    // NO_LOCK: skip taking /var/lib/pacman/db.lck — we're a non-root reader,
    // and the real pacman invocation will take the lock for the actual write.
    // NEEDED: mirror the real `--needed` flag so we don't flag a conflict for
    // a same-version reinstall that pacman would silently skip.
    alpm.trans_init(TransFlag::NO_LOCK | TransFlag::NEEDED)
        .map_err(|e| Error::other(format!("alpm trans_init: {e}")))?;
    for path in paths {
        let loaded = alpm
            .pkg_load(*path, true, SigLevel::NONE)
            .map_err(|e| Error::other(format!("alpm pkg_load {path}: {e}")))?;
        alpm.trans_add_pkg(loaded)
            .map_err(|e| Error::other(format!("alpm trans_add_pkg {path}: {}", e.error)))?;
    }
    // PrepareError borrows `alpm`, so it must be dropped before
    // trans_release (which needs `&mut alpm`). Report in-arm, then drop.
    let outcome = if let Err(prep_err) = alpm.trans_prepare() {
        report_preflight_failure(&prep_err);
        PreflightOutcome::Flagged
    } else {
        PreflightOutcome::Clean
    };
    alpm.trans_release().ok();
    Ok(outcome)
}

/// Log each prepare-time complaint as its own structured warn event so the
/// fields (`pkg1`, `pkg2`, `reason`, …) are queryable in the log.
fn report_preflight_failure(err: &PrepareError<'_>) {
    let Some(data) = err.data() else {
        warn!(error = %err, "pacman preflight: prepare failed without detail");
        return;
    };
    match data {
        PrepareData::ConflictingDeps(list) => {
            for c in list {
                warn!(
                    pkg1 = c.package1().name(),
                    pkg2 = c.package2().name(),
                    reason = %c.reason(),
                    "pacman preflight: conflict detected",
                );
            }
        }
        PrepareData::UnsatisfiedDeps(list) => {
            for m in list {
                warn!(
                    target = m.target(),
                    depend = %m.depend(),
                    causing_pkg = m.causing_pkg().unwrap_or("(none)"),
                    "pacman preflight: unsatisfied dep",
                );
            }
        }
        PrepareData::PkgInvalidArch(list) => {
            for p in list {
                warn!(
                    pkg = p.name(),
                    arch = p.arch().unwrap_or("(unknown)"),
                    "pacman preflight: invalid architecture",
                );
            }
        }
    }
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

    #[test]
    fn tee_pipe_forwards_and_captures() {
        let input = b"error: target not found\nerror: dep 'foo' was not found\n";
        let mut sink = Vec::new();
        let captured = tee_pipe(std::io::Cursor::new(input.to_vec()), &mut sink);
        assert_eq!(sink, input);
        assert_eq!(captured, std::str::from_utf8(input).unwrap());
    }

    #[test]
    fn tee_pipe_handles_empty_stream() {
        let mut sink = Vec::new();
        let captured = tee_pipe(std::io::Cursor::new(Vec::<u8>::new()), &mut sink);
        assert!(sink.is_empty());
        assert!(captured.is_empty());
    }

    #[test]
    fn dash_u_paths_extracts_files_only() {
        let argv: Vec<String> = [
            "-U",
            "--needed",
            "--noconfirm",
            "/p/a.pkg.tar.zst",
            "/p/b.pkg.tar.zst",
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let paths = dash_u_paths(&argv).expect("argv has -U");
        assert_eq!(paths, vec!["/p/a.pkg.tar.zst", "/p/b.pkg.tar.zst"]);
    }

    #[test]
    fn dash_u_paths_returns_none_without_dash_u() {
        let argv: Vec<String> = ["-Syu", "--noconfirm"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert!(dash_u_paths(&argv).is_none());
    }

    #[test]
    fn dash_u_paths_empty_when_no_files() {
        let argv: Vec<String> = ["-U", "--needed"].iter().map(|s| (*s).to_owned()).collect();
        let paths = dash_u_paths(&argv).expect("argv has -U");
        assert!(paths.is_empty());
    }

    #[test]
    fn clean_capture_strips_ansi_and_trims_newlines() {
        // pacman colors errors red; the SGR sequences must not leak into the log.
        let stdout = "loading packages...\nresolving dependencies...\n";
        let stderr = "\x1b[1;31merror: \x1b[0munresolvable package conflicts detected\n";
        let cleaned = clean_pacman_capture(stdout, stderr).expect("non-empty input");
        assert_eq!(
            cleaned.stdout,
            "loading packages...\nresolving dependencies..."
        );
        assert_eq!(
            cleaned.stderr,
            "error: unresolvable package conflicts detected"
        );
        assert!(
            !cleaned.stderr.contains('\x1b'),
            "raw escape leaked: {:?}",
            cleaned.stderr,
        );
    }

    #[test]
    fn clean_capture_returns_none_when_both_empty() {
        assert!(clean_pacman_capture("", "").is_none());
        // Newline-only is still empty after trim — don't emit a noise event.
        assert!(clean_pacman_capture("\n", "\n\n").is_none());
    }

    #[test]
    fn clean_capture_keeps_one_channel_when_other_empty() {
        let cleaned = clean_pacman_capture("", "error: something\n").expect("non-empty stderr");
        assert!(cleaned.stdout.is_empty());
        assert_eq!(cleaned.stderr, "error: something");
    }

    #[test]
    fn tee_pipe_handles_non_utf8_bytes() {
        let input = b"warning: \xff\xfe bad bytes\n";
        let mut sink = Vec::new();
        let captured = tee_pipe(std::io::Cursor::new(input.to_vec()), &mut sink);
        assert_eq!(sink, input);
        assert!(captured.contains("warning:"));
        assert!(captured.contains("bad bytes"));
    }
}
