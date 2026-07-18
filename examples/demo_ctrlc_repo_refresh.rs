//! Demo driver: Ctrl-C during the official-repo DB refresh aborts the
//! download *instantly* and bails back to the prompt.
//!
//! The seed (`demos/seed-ctrlc-repo-refresh.sh`) re-points pacman.conf's
//! `[local-repo]` at [`hung_mirror`](./hung_mirror.rs) — a server that
//! answers the response headers then goes silent — so `refresh pacman` parks
//! mid-download inside libalpm's update. libalpm's own downloader cannot be
//! interrupted from outside (pacman itself `_Exit`s on ^C); aurox's fetch
//! callback (`pacman/dload.rs`) is what makes the ^C land: its curl progress
//! meter watches the SIGINT flag and aborts the transfer within a beat. A
//! second ^C at the now-idle prompt leaves the shell (exit 130) — the ^C ^C
//! muscle-memory path, end to end.
//!
//! Rendered to `docs/demo/ctrlc-repo-refresh.gif` by `demos/build.sh`; run as
//! a plain test by `tests/container/extended/39_demo_ctrlc_repo_refresh.sh`.

use pty_harness::{Pty, dwell};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// Must match the `Server = http://127.0.0.1:<PORT>` the seed writes into
/// pacman.conf.
const PORT: u16 = 18791;

fn main() {
    enable_repo_sync();
    // Started before the shell so it is listening when the refresh dials in.
    let mut server = spawn_hung_mirror();
    dwell(300);

    let mut pty = Pty::spawn_aurox();
    pty.expect("shell banner", |s| s.contains("aurox shell"));
    dwell(1500);

    pty.send_human("refresh pacman");
    // The DB's byte-row is on screen: the transfer is in flight — and parked,
    // since the server sent headers then went silent.
    pty.expect("db download started", |s| s.contains("local-repo.db"));
    // Let the viewer sit with the stalled row before the interrupt.
    dwell(2500);

    // The user's Ctrl-C, as the terminal delivers it.
    let interrupted_at = Instant::now();
    pty.send(&[0x03]);
    pty.expect("refresh reported interrupted", |s| {
        s.contains("official-repo refresh interrupted")
    });
    let waited = interrupted_at.elapsed();
    assert!(
        waited < Duration::from_secs(8),
        "interrupt took {waited:?} — it must abort the transfer, not wait it out"
    );
    // Hold the live prompt on screen: the shell survived the abort.
    dwell(2500);

    // And the idle-prompt ^C leaves the shell with the interrupt-quit code.
    pty.send(&[0x03]);
    pty.finish_with_code(130);

    server.kill().ok();
    server.wait().ok();
    println!("DEMO_CTRLC_REPO_REFRESH_OK");
}

/// Flip `check_repo_updates` on in the config the container harness wrote.
/// The suite default keeps the repo sync off so unrelated tests stay off the
/// network — this demo is exactly about that sync, and flipping it here (not
/// in the seed) keeps the mirror-bootstrap `aurox -Sy` before the driver from
/// dialing the hung server.
fn enable_repo_sync() {
    let path = config_dir().join("config.toml");
    let cfg =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let flipped = cfg.replace("check_repo_updates = false", "check_repo_updates = true");
    assert_ne!(
        flipped, cfg,
        "config.toml did not carry check_repo_updates = false"
    );
    std::fs::write(&path, flipped).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

/// Launch `hung_mirror`, which lives next to this driver in the examples dir.
fn spawn_hung_mirror() -> Child {
    let bin = current_exe_dir().join("hung_mirror");
    Command::new(&bin)
        .arg(PORT.to_string())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()))
}

fn current_exe_dir() -> PathBuf {
    std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("exe has a parent dir")
        .to_path_buf()
}

/// Mirror of aurox's `paths::config_dir()` — `$XDG_CONFIG_HOME/aurox` (or
/// `~/.config/aurox`), so the driver rewrites the same file the shell reads.
fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map_or_else(
            || PathBuf::from(std::env::var_os("HOME").expect("HOME set")).join(".config"),
            PathBuf::from,
        )
        .join("aurox")
}
