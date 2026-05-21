//! Regression test for the upgrade picker's redraw behaviour.
//!
//! The original bug: dialoguer 0.11's `clear_preserve_prompt` uses
//! `items[i].len()` (raw bytes) to estimate wrap. With ANSI-coloured rows
//! that byte count vastly exceeds the visible width, so every redraw after
//! an arrow-down would `clear_last_lines(too_many)` — overwriting whatever
//! had been printed above the prompt (typically log lines).
//!
//! Fix: feed dialoguer plain-ASCII labels and recolour at draw time via
//! `UpgradePickerTheme` (see `src/ui.rs`). dialoguer's wrap math now sees
//! plain widths, so the clear count matches what was drawn.
//!
//! This test drives the headless `picker_e2e` example inside a real PTY,
//! optionally wrapped in `podman` for a pinned `$TERM`/locale/size, prints
//! sentinel lines before the picker, scrolls past the visible area, and
//! asserts the sentinels are still on screen afterwards. Runs only with
//! `cargo test -- --ignored` because it shells out to `cargo build` and
//! optionally `podman`.

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use vt100::Parser;

/// Tall enough that `SENTINEL_COUNT` sentinels + prompt + 20 items + a few
/// blank rows all fit on screen at once — otherwise the top sentinels
/// scroll off naturally (not due to dialoguer) and the assertion becomes
/// a false positive. Width is chosen so the **plain** row (≈ 35 chars +
/// 6-char prefix) fits comfortably, but the **ANSI-laden** row (~100
/// bytes) does NOT — that's the exact condition that used to trip
/// dialoguer's wrap math (`item.len() > term.cols`) and have it
/// over-clear lines above the prompt on every redraw.
const PTY_ROWS: u16 = 32;
const PTY_COLS: u16 = 80;
const SENTINEL_COUNT: usize = 5;
/// Each arrow-down triggers a `clear_preserve_prompt` redraw — the path
/// that used to over-clear with ANSI items. Ten is plenty to surface a
/// per-row over-count without wrapping the cursor back to the top.
const SCROLL_KEYS: usize = 10;

#[test]
#[ignore = "spawns cargo/podman + PTY; run with `cargo test -- --ignored`"]
fn picker_redraw_preserves_lines_above() {
    let example = ensure_example_built();
    let Some((program, args)) = resolve_runner(&example) else {
        eprintln!(
            "SKIP: GITAUR_E2E_PICKER=podman selected but `podman` or the \
             `gitaur-test:latest` image is unavailable; run \
             tests/container/run.sh --rebuild first, or unset \
             GITAUR_E2E_PICKER to fall back to host PTY."
        );
        return;
    };

    let pty = NativePtySystem::default()
        .openpty(PtySize {
            rows: PTY_ROWS,
            cols: PTY_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let mut cmd = CommandBuilder::new(&program);
    for a in &args {
        cmd.arg(a);
    }
    cmd.env("PICKER_E2E_SENTINELS", SENTINEL_COUNT.to_string());
    cmd.env("TERM", "xterm-256color");
    cmd.env("NO_COLOR", ""); // make sure nothing in the chain forces it off

    let mut child = pty.slave.spawn_command(cmd).expect("spawn child");
    drop(pty.slave);

    let reader = pty.master.try_clone_reader().expect("clone reader");
    let mut writer = pty.master.take_writer().expect("take writer");
    let rx = spawn_reader_thread(reader);

    let mut parser = Parser::new(PTY_ROWS, PTY_COLS, 0);

    // Wait until the *last* item row is on screen. Just checking for the
    // prompt fires too early — items are still streaming and a keypress
    // sent at that moment races the initial draw.
    pump_until(
        &mut parser,
        &rx,
        |p| screen_text(p).contains("pkg-18"),
        Duration::from_secs(20),
    )
    .expect("picker never finished its first render");

    // Scroll. Each arrow-down triggers a `clear_preserve_prompt` + redraw
    // — the exact path that used to over-clear with ANSI items.
    for _ in 0..SCROLL_KEYS {
        writer.write_all(b"\x1b[B").expect("write arrow-down");
        writer.flush().ok();
        pump_for(&mut parser, &rx, Duration::from_millis(60));
    }

    let after_scroll = screen_text(&parser);

    // Submit & drain so the child exits cleanly (best-effort; we've already
    // captured the screen state we care about).
    writer.write_all(b"\r").ok();
    drop(writer);
    pump_for(&mut parser, &rx, Duration::from_secs(2));
    let _ = child.wait();

    for i in 1..=SENTINEL_COUNT {
        let needle = format!("SENTINEL-{i:02}");
        assert!(
            after_scroll.contains(&needle),
            "{needle} missing from screen after {SCROLL_KEYS} arrow-down keys.\n\
             dialoguer over-cleared the lines above the prompt — the redraw \
             regression is back.\n\n--- screen ---\n{after_scroll}\n--- end ---"
        );
    }
}

fn ensure_example_built() -> PathBuf {
    let status = std::process::Command::new(env!("CARGO"))
        .args(["build", "--example", "picker_e2e", "--quiet"])
        .status()
        .expect("invoke cargo build");
    assert!(status.success(), "cargo build --example picker_e2e failed");

    let target_dir = std::env::var("CARGO_TARGET_DIR").map_or_else(
        |_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target"),
        PathBuf::from,
    );
    let path = target_dir.join("debug/examples/picker_e2e");
    assert!(path.exists(), "missing built example: {}", path.display());
    path
}

/// Decide whether to run the example directly or inside `podman`. Default
/// is host PTY (no extra setup). Set `GITAUR_E2E_PICKER=podman` to require
/// the container path (returns `None` if podman or the image isn't there,
/// so the caller can skip cleanly).
fn resolve_runner(example: &std::path::Path) -> Option<(String, Vec<String>)> {
    match std::env::var("GITAUR_E2E_PICKER").as_deref() {
        Ok("podman") => {
            if !podman_image_present() {
                return None;
            }
            let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            // Mount the repo so the just-built example is visible inside
            // the container at the same relative path.
            let inside_path = format!(
                "/work/{}",
                example
                    .strip_prefix(&repo)
                    .expect("example under manifest dir")
                    .display()
            );
            let args = vec![
                "run".into(),
                "--rm".into(),
                "-i".into(),
                "-t".into(),
                "-v".into(),
                format!("{}:/work:ro", repo.display()),
                "-w".into(),
                "/work".into(),
                "-e".into(),
                format!("PICKER_E2E_SENTINELS={SENTINEL_COUNT}"),
                "gitaur-test:latest".into(),
                inside_path,
            ];
            Some(("podman".into(), args))
        }
        _ => Some((example.to_string_lossy().into_owned(), Vec::new())),
    }
}

fn podman_image_present() -> bool {
    if std::process::Command::new("podman")
        .arg("--version")
        .output()
        .is_err()
    {
        return false;
    }
    std::process::Command::new("podman")
        .args(["image", "exists", "gitaur-test:latest"])
        .status()
        .is_ok_and(|s| s.success())
}

/// Spawn a thread that pulls bytes off the PTY master into a channel. Set
/// `PICKER_E2E_RAW_DUMP=/path/to/file` to also mirror the stream to disk
/// for offline inspection — useful when the assertion fails and you want
/// to see the exact `\x1b[NA` (cursor-up) counts dialoguer emitted.
fn spawn_reader_thread(mut reader: Box<dyn Read + Send>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let mut sink = std::env::var_os("PICKER_E2E_RAW_DUMP").and_then(|p| {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(p)
                .ok()
        });
        loop {
            match reader.read(&mut buf) {
                Ok(n) if n > 0 => {
                    if let Some(f) = sink.as_mut() {
                        let _ = std::io::Write::write_all(f, &buf[..n]);
                    }
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
    });
    rx
}

fn screen_text(parser: &Parser) -> String {
    parser.screen().contents()
}

fn pump_for(parser: &mut Parser, rx: &mpsc::Receiver<Vec<u8>>, dur: Duration) {
    let deadline = Instant::now() + dur;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(bytes) => parser.process(&bytes),
            Err(_) => break,
        }
    }
}

fn pump_until<F>(
    parser: &mut Parser,
    rx: &mpsc::Receiver<Vec<u8>>,
    mut done: F,
    overall: Duration,
) -> Result<(), String>
where
    F: FnMut(&Parser) -> bool,
{
    let deadline = Instant::now() + overall;
    loop {
        if done(parser) {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "predicate not satisfied within {overall:?}; screen so far:\n{}",
                screen_text(parser)
            ));
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(bytes) => parser.process(&bytes),
            Err(mpsc::RecvTimeoutError::Timeout) => {} // re-check predicate
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(format!(
                    "PTY closed before predicate satisfied; screen so far:\n{}",
                    screen_text(parser)
                ));
            }
        }
    }
}
