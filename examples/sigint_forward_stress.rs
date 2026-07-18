//! Reproduction stress for issue #59 — the fork-window SIGINT swallow that
//! intermittently hung Ctrl-C'd builds.
//!
//! This is `build::makepkg::run`'s interrupt dance with everything else
//! stripped away: a child under a fresh PTY printing `READY` then sleeping
//! (the `test-sleep-build` fixture in miniature), a reader thread pumping
//! the PTY master (the `tee`), a signal-hook watcher that flips a flag and
//! forwards SIGINT to the child's process group, and the main flow blocked
//! in `child.wait()`. Each round delivers a group-directed SIGINT to our own
//! process group — the same kernel path a terminal's `^C` takes to the
//! foreground group — and requires `wait()` to return promptly with the
//! flag set.
//!
//! Three modes tell the root-cause story:
//!
//!   * default (untrapped `bash -c`) — the negative control: a SIGINT
//!     landing in the child's fork→exec window kills the pre-exec child
//!     under `SIG_DFL`, visibly. Never hangs.
//!   * `--trap` — makepkg's shape: the same window-hit is *consumed* under
//!     the inherited trap disposition (`execve` discards the handler's
//!     flag), `sleep` starts unsignalled, and bash defers its trap until
//!     that child exits — never. Hangs ~5% of rounds even on an idle host.
//!   * `--trap --verified` — the fix under test: the watcher re-forwards
//!     while the condemned group still lives, exactly
//!     `build::makepkg::forward_until_exit`'s loop. 0 hangs expected.
//!
//! A stuck round is classified before recovery (watcher flag, child pgid
//! from `/proc`, per-thread signal masks/pending sets, a one-shot manual
//! re-forward that discriminates wakeup-loss from forward-failure), so any
//! *new* loss mode reports enough to diagnose itself.
//!
//! Usage: `cargo run --example sigint_forward_stress -- [--trap]
//! [--verified] [rounds]` (default 200). Exits non-zero if any round hung,
//! printing one `LOST_FORWARD …` line per incident.

// std's scope, deliberately: this repro mirrors the *libraries*' behavior
// (signal-hook, portable-pty, the kernel) and must not pull in the crate's
// tracing-context plumbing — there is no subscriber here to propagate.
#![allow(clippy::disallowed_methods)]

use nix::sys::signal::{Signal, killpg};
use nix::unistd::{Pid, getpgrp, setpgid};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use signal_hook::consts::SIGINT;
use signal_hook::iterator::Signals;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long a round may take from SIGINT to `wait()` returning before it is
/// declared lost. Normal rounds finish in tens of milliseconds even under
/// load; the real-world failure blew a 45 s window.
const LOST_AFTER: Duration = Duration::from_secs(8);

/// Cap on waiting for the child's READY sentinel (slow spawns under load).
const READY_AFTER: Duration = Duration::from_secs(15);

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let trapped = args.peek().is_some_and(|a| a == "--trap");
    if trapped {
        args.next();
    }
    let verified = args.peek().is_some_and(|a| a == "--verified");
    if verified {
        args.next();
    }
    let rounds: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);

    // Our own process group, so the group-directed SIGINT below can't reach
    // the invoking cargo/shell (which still have SIG_DFL and would die).
    setpgid(Pid::from_raw(0), Pid::from_raw(0)).expect("setpgid");

    let mut lost = 0usize;
    for round in 0..rounds {
        lost += usize::from(!one_round(round, trapped, verified));
    }
    if lost > 0 {
        println!("FAILED: {lost}/{rounds} rounds lost the forward");
        std::process::exit(1);
    }
    println!("OK: {rounds} rounds, every forward landed");
}

/// One full spawn → sentinel → SIGINT → interrupted-wait cycle. Returns
/// whether the forward landed within [`LOST_AFTER`]. Mirrors
/// `build::makepkg::run` structurally: same crate versions, same
/// spawn-under-pty, same `Signals` + watcher + `killpg`, same drop order.
/// The SIGINT fires only once the round is armed (inner child sleeping,
/// watcher live): between rounds no `Signals` action is registered, and
/// signal-hook's permanent dispatcher would swallow an early delivery — an
/// artificial loss the real code can't hit.
fn one_round(round: usize, trapped: bool, verified: bool) -> bool {
    let pty = NativePtySystem::default()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new("bash");
    if trapped {
        // makepkg's shape: bash with an INT trap. This is the root-cause
        // ingredient (#59): the READY echo is the starting gun for both our
        // SIGINT and bash's fork of `sleep`. A SIGINT landing in that child's
        // fork→exec window is consumed under the inherited trap disposition
        // (the handler flag it sets dies with `execve`), `sleep` starts
        // untouched, and bash — per its wait-and-cooperative-exit rule —
        // defers the trap until its foreground child exits: never. An
        // untrapped child turns the same window-hit into a visible kill
        // (default disposition pre-exec), which is why the plain mode can't
        // reproduce the hang.
        cmd.args([
            "-c",
            "trap 'echo TRAP-EXIT; exit 130' INT; echo READY; sleep 600",
        ]);
    } else {
        cmd.args(["-c", "echo READY; sleep 600"]);
    }
    let mut child = pty.slave.spawn_command(cmd).expect("spawn under pty");
    drop(pty.slave);
    let pid = child.process_id();
    let mut reader = pty.master.try_clone_reader().expect("clone reader");

    let mut signals = Signals::new([SIGINT]).expect("Signals::new");
    let handle = signals.handle();
    let interrupted = AtomicBool::new(false);
    // Set once `child.wait()` has returned — the watchdog's view of progress.
    let done = AtomicBool::new(false);
    let (ready_tx, ready_rx) = mpsc::channel::<()>();
    // `--verified` only: closed when `wait()` returns, unblocking the
    // watcher's re-forward loop — the fix under test, same shape as
    // `build::makepkg::run`'s.
    let (exited_tx, exited_rx) = mpsc::channel::<()>();

    let ok = std::thread::scope(|s| {
        // The `tee`: pump the master until EOF, announcing the sentinel once.
        s.spawn(move || {
            let mut seen = Vec::new();
            let mut buf = [0u8; 4096];
            let mut announced = false;
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if !announced {
                    seen.extend_from_slice(&buf[..n]);
                    if seen.windows(5).any(|w| w == b"READY") {
                        announced = true;
                        ready_tx.send(()).ok();
                    }
                }
            }
        });
        // The watcher — verbatim from `makepkg::run`. With `--verified`, the
        // fixed shape: after forwarding, block on the exit channel with a
        // grace timeout and re-forward while the condemned group still lives
        // (a fork-window swallow becomes a one-beat delay).
        let signals_iter = &mut signals;
        let interrupted_ref = &interrupted;
        s.spawn(move || {
            for _ in signals_iter {
                interrupted_ref.store(true, Ordering::SeqCst);
                forward_sigint(pid);
                if verified {
                    while exited_rx.recv_timeout(Duration::from_millis(300))
                        == Err(mpsc::RecvTimeoutError::Timeout)
                    {
                        forward_sigint(pid);
                    }
                    break;
                }
            }
        });
        // The watchdog: classify and recover a stuck round so the stress can
        // keep going and count incidents instead of hanging.
        let watchdog = s.spawn(|| watchdog(round, pid, &done, &interrupted));

        if ready_rx.recv_timeout(READY_AFTER).is_err() {
            println!("LOST_FORWARD round={round} phase=ready-timeout (spawn never printed READY)");
            // Unstick: no signal was sent yet, so just kill the child.
            kill_group(pid, Signal::SIGKILL);
        } else {
            // The terminal's `^C`: a group-directed SIGINT to our own group.
            killpg(getpgrp(), Signal::SIGINT).expect("killpg self group");
        }
        child.wait().expect("wait child");
        drop(exited_tx);
        done.store(true, Ordering::SeqCst);
        handle.close();
        watchdog.join().expect("watchdog panicked")
    });
    ok && interrupted.load(Ordering::SeqCst)
}

/// Wait out [`LOST_AFTER`]; on expiry print the classification dump, retry
/// the forward once, and finally SIGKILL the child group so the round can
/// end. Returns whether the round was clean.
fn watchdog(round: usize, pid: Option<u32>, done: &AtomicBool, interrupted: &AtomicBool) -> bool {
    let deadline = Instant::now() + LOST_AFTER;
    while Instant::now() < deadline {
        if done.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let flag = interrupted.load(Ordering::SeqCst);
    let fmt_opt = |v: Option<i32>| v.map_or_else(|| "none".to_owned(), |v| v.to_string());
    println!(
        "LOST_FORWARD round={round} flag={flag} child={} child_pgid={}",
        fmt_opt(pid.and_then(|p| i32::try_from(p).ok())),
        fmt_opt(pid.and_then(child_pgid)),
    );
    dump_signal_state();
    // Discriminator: does a second, manual forward unstick it?
    forward_sigint(pid);
    let retry_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < retry_deadline {
        if done.load(Ordering::SeqCst) {
            println!("LOST_FORWARD round={round} RECOVERED_BY_RETRY");
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("LOST_FORWARD round={round} RETRY_INEFFECTIVE — SIGKILLing child group");
    kill_group(pid, Signal::SIGKILL);
    false
}

/// Verbatim copy of `build::makepkg::forward_sigint`, with stdout reporting
/// instead of tracing (this repro has no subscriber).
fn forward_sigint(pid: Option<u32>) {
    let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) else {
        println!("forward: no child pid to forward SIGINT to");
        return;
    };
    if let Err(e) = killpg(Pid::from_raw(pid), Signal::SIGINT) {
        println!("forward: killpg({pid}) failed: {e}");
    }
}

fn kill_group(pid: Option<u32>, sig: Signal) {
    if let Some(pid) = pid.and_then(|p| i32::try_from(p).ok()) {
        killpg(Pid::from_raw(pid), sig).ok();
    }
}

/// The child's *actual* process group id, from `/proc/<pid>/stat` field 5 —
/// whether the `killpg(pid)` target assumption (`pgid == pid`, courtesy of
/// portable-pty's `setsid`) held.
fn child_pgid(pid: u32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised comm (which may itself contain spaces).
    let rest = stat.rsplit_once(')')?.1;
    rest.split_whitespace().nth(2)?.parse().ok()
}

/// Per-thread signal masks and pending sets — the discriminator for a lost
/// wakeup: `ShdPnd` carrying bit 2 (SIGINT) with every thread's `SigBlk`
/// masking it means the signal is parked, undeliverable; all-zero pending
/// means it was consumed and vanished downstream.
fn dump_signal_state() {
    let read =
        |p: &str| std::fs::read_to_string(p).unwrap_or_else(|_| String::from("<unreadable>"));
    for line in read("/proc/self/status").lines() {
        if line.starts_with("ShdPnd") || line.starts_with("SigPnd") {
            println!("  self {line}");
        }
    }
    let Ok(tasks) = std::fs::read_dir("/proc/self/task") else {
        return;
    };
    for t in tasks.flatten() {
        let tid = t.file_name();
        let status = read(&format!("/proc/self/task/{}/status", tid.to_string_lossy()));
        let grab = |k: &str| {
            status
                .lines()
                .find(|l| l.starts_with(k))
                .unwrap_or("")
                .to_owned()
        };
        println!(
            "  tid={} {} {} {}",
            tid.to_string_lossy(),
            grab("SigPnd"),
            grab("SigBlk"),
            grab("SigCgt"),
        );
    }
}
