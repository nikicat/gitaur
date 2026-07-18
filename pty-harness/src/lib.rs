//! Shared PTY harness for the aurox e2e example drivers (`upgrade_loop_e2e`,
//! `loop_built_tag_e2e`, …).
//!
//! The upgrade loop only runs interactively (stdin must be a TTY), so each
//! driver spawns the real `aurox` binary under a PTY, parses its VT100 output
//! into a screen grid, and walks the expected UI sequence. The mechanics —
//! spawn, read pump, [`Pty::expect`]/[`Pty::send`], clean teardown — are
//! identical across scenarios; only the sequence of expectations differs.
//!
//! This lives in its own crate, pulled in as a path **dev-dependency**, rather
//! than as a module inside one example: an example is a bin crate with no
//! external API, so a shared module there can't satisfy both `unreachable_pub`
//! (no bare `pub`) and `clippy::redundant_pub_crate` (no `pub(crate)` in a
//! private module). Here the drivers are genuine external users, so the API is
//! plainly `pub` and neither lint applies. Each scenario stays a small example
//! that `use pty_harness::Pty;` and scripts its own flow — adding one is a new
//! file, not a branch in a growing dispatch.

use cast::CastRecorder;
use portable_pty::{Child, CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use vt100::Parser;

mod cast;

const ROWS: u16 = 40;
const COLS: u16 = 100;

/// A spawned `aurox` under a PTY, with its screen parser and I/O channels.
///
/// `_master` is held only to keep the PTY open for the process's lifetime —
/// the reader/writer are derived from it.
/// How a bounded [`Pty::try_expect`] watch resolved. A dedicated tri-state
/// rather than a bool: for a probe, "aurox exited" is a different finding
/// than "still running but silent" — collapsing them is what cost issue
/// #59's first failures their diagnosis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// The predicate held over the screen.
    Matched,
    /// The deadline passed without the predicate ever holding.
    TimedOut,
    /// aurox exited before the predicate held (the screen stays readable).
    Exited,
}

pub struct Pty {
    parser: Parser,
    rx: mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
    /// Typing-jitter RNG for [`Self::send_human`] — fixed seed, so a demo's
    /// keystroke rhythm is the same on every run.
    rng: fastrand::Rng,
}

impl Pty {
    /// Spawn `aurox` (from argv[1], else `$AUROX`, else the default debug path)
    /// with no args — the upgrade loop — inheriting the container env so it
    /// finds its config, the mock mirror, pacman, sudo, and makepkg.
    pub fn spawn_aurox() -> Self {
        Self::spawn_aurox_args(&[])
    }

    /// Like [`Self::spawn_aurox`] but passes `args` to `aurox`. Used to drive the
    /// bare-term launch (`aurox <term>…`), which opens the shell *seeded* with
    /// that `search` instead of the plain upgrade-loop prompt.
    pub fn spawn_aurox_args(args: &[&str]) -> Self {
        let aurox = resolve_aurox();
        let mut cmd = CommandBuilder::new(&aurox);
        for a in args {
            cmd.arg(a);
        }
        let title = if args.is_empty() {
            "aurox".to_owned()
        } else {
            format!("aurox {}", args.join(" "))
        };
        Self::spawn(cmd, &[], &title)
    }

    /// An interactive bash under the PTY, for demo drivers that showcase a
    /// CLI invocation — typing `aurox -S …` at a shell prompt is then part of
    /// the recording, not off-screen argv. `--norc` keeps the session
    /// hermetic; `PS1` is a minimal colored `❯`, and the resolved aurox
    /// binary's directory is prepended to `PATH` so the typed command is a
    /// bare `aurox`.
    pub fn spawn_demo_shell() -> Self {
        let aurox = resolve_aurox();
        let bin_dir = std::path::Path::new(&aurox)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let path = format!("{bin_dir}:{}", std::env::var("PATH").unwrap_or_default());
        let mut cmd = CommandBuilder::new("bash");
        cmd.arg("--norc");
        cmd.arg("-i");
        let overrides = [
            // \[…\] wraps the color codes as zero-width for readline.
            ("PS1", "\\[\\e[1;36m\\]\u{276F}\\[\\e[0m\\] ".to_owned()),
            ("PATH", path),
        ];
        Self::spawn(cmd, &overrides, "demo shell")
    }

    /// Common spawn tail: inherit the container env (so aurox finds its
    /// config, the mock mirror, pacman, sudo, and makepkg), pin `TERM`, apply
    /// caller `overrides` last so inheritance can't clobber them, and wire up
    /// the PTY, reader thread, and (env-gated) cast recorder.
    fn spawn(mut cmd: CommandBuilder, overrides: &[(&str, String)], title: &str) -> Self {
        let pty = NativePtySystem::default()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("openpty");

        for (k, v) in std::env::vars() {
            cmd.env(k, v);
        }
        cmd.env("TERM", "xterm-256color");
        // The test image's Dockerfile sets `RUST_LOG=off` so the console
        // tracing layer doesn't share this PTY with the UI we assert on (a
        // stray WARN floods the screen). All assertable output comes from
        // `ui::*` eprintlns, which run regardless of the tracing filter.
        for (k, v) in overrides {
            cmd.env(k, v);
        }

        let child = pty.slave.spawn_command(cmd).expect("spawn under pty");
        drop(pty.slave);

        let reader = pty.master.try_clone_reader().expect("clone reader");
        let writer = pty.master.take_writer().expect("take writer");
        Self {
            parser: Parser::new(ROWS, COLS, 0),
            rx: spawn_reader(reader, CastRecorder::from_env(title)),
            writer,
            child,
            _master: pty.master,
            rng: fastrand::Rng::with_seed(0x5EED),
        }
    }

    /// The current screen contents as plain text (ANSI already interpreted).
    pub fn screen(&self) -> String {
        self.parser.screen().contents()
    }

    /// Pump the PTY until `pred` holds over the screen, or panic with the
    /// screen on a 45s timeout (or if `aurox` exits first).
    pub fn expect<F>(&mut self, what: &str, mut pred: F)
    where
        F: FnMut(&str) -> bool,
    {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if pred(&self.parser.screen().contents()) {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for {what}\n--- screen ---\n{}\n--- end ---",
                self.parser.screen().contents()
            );
            match self
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(200)))
            {
                Ok(bytes) => self.parser.process(&bytes),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                    "aurox exited before {what} appeared\n--- screen ---\n{}\n--- end ---",
                    self.parser.screen().contents()
                ),
            }
        }
    }

    /// Non-panicking [`Self::expect`] with a caller-chosen deadline: pump the
    /// PTY until `pred` holds and report how the watch resolved. For probe
    /// drivers that classify a failure and keep interrogating the session
    /// (issue #59's second-`^C` probe) instead of dying on the first miss.
    /// The screen stays readable via [`Self::screen`] on every outcome.
    pub fn try_expect<F>(&mut self, timeout: Duration, mut pred: F) -> Expectation
    where
        F: FnMut(&str) -> bool,
    {
        let deadline = Instant::now() + timeout;
        loop {
            if pred(&self.parser.screen().contents()) {
                return Expectation::Matched;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Expectation::TimedOut;
            }
            match self
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(200)))
            {
                Ok(bytes) => self.parser.process(&bytes),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Expectation::Exited,
            }
        }
    }

    /// Write bytes to the PTY (e.g. `b"\r"` to confirm a prompt).
    pub fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).expect("write to pty");
        self.writer.flush().ok();
    }

    /// Demo pacing: type `line` character by character with a human-ish,
    /// *deterministic* rhythm, then Enter after a beat. rustyline echoes each
    /// keystroke, so in a cast recording this reads as live typing. Only call
    /// at a prompt (same ack rule as [`Self::send`] — buffered input sent
    /// before rustyline reads is dropped); the per-char trickle itself is
    /// what a terminal delivers anyway.
    pub fn send_human(&mut self, line: &str) {
        let mut buf = [0u8; 4];
        for c in line.chars() {
            self.send(c.encode_utf8(&mut buf).as_bytes());
            std::thread::sleep(Duration::from_millis(self.rng.u64(35..80)));
        }
        std::thread::sleep(Duration::from_millis(180));
        self.send(b"\r");
    }

    /// Close the input, drain remaining output, and assert `aurox` exited 0.
    /// Consumes the harness — the scenario is over.
    pub fn finish_clean(self) {
        let Self {
            mut parser,
            rx,
            writer,
            mut child,
            _master,
            rng: _,
        } = self;
        drop(writer);
        pump_for(&mut parser, &rx, Duration::from_secs(5));
        let status = child.wait().expect("wait aurox");
        assert!(
            status.success(),
            "aurox exited non-zero ({status:?})\n--- screen ---\n{}",
            parser.screen().contents()
        );
    }

    /// Kill `aurox` and reap it — for scenarios whose assertion is complete once
    /// a screen rendered, with no clean exit path to drive.
    pub fn kill(mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// The aurox binary under test: argv[1] → `$AUROX` → the default debug path.
fn resolve_aurox() -> String {
    std::env::args()
        .nth(1)
        .or_else(|| std::env::var("AUROX").ok())
        .unwrap_or_else(|| "/work/target/debug/aurox".to_owned())
}

/// Demo pacing: hold the current screen so a viewer can read it. Output that
/// arrives meanwhile is still pumped into the cast by the reader thread with
/// true timing; only the driver waits.
pub fn dwell(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

/// True when the [`Pty::spawn_demo_shell`] prompt is the last non-blank line
/// — the foreground command finished and bash is reading again. Counting `❯`
/// occurrences breaks once earlier prompt lines scroll off the vt100 grid.
pub fn back_at_prompt(screen: &str) -> bool {
    screen
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.trim() == "\u{276F}")
}

/// Whitespace-insensitive containment: table columns pad to the widest staged
/// row and long lines wrap on the 100-col vt100 grid, so a literal
/// `1.0-1 → 2.0-1` match breaks whenever padding widths or the wrap position
/// shift. Compacting both sides makes the match immune to both.
pub fn has(screen: &str, needle: &str) -> bool {
    let compact = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
    compact(screen).contains(&compact(needle))
}

fn pump_for(parser: &mut Parser, rx: &mpsc::Receiver<Vec<u8>>, dur: Duration) {
    let deadline = Instant::now() + dur;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        match rx.recv_timeout(remaining) {
            Ok(bytes) => parser.process(&bytes),
            Err(_) => return,
        }
    }
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    mut recorder: Option<CastRecorder>,
) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    // pty-harness is a standalone dev crate with no aurox thread-locals to
    // propagate, so the `context::spawn` rule (src/context.rs) doesn't apply.
    #[allow(clippy::disallowed_methods)]
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            // Tee into the cast here, at read time, so event timing reflects
            // when output appeared — not when `expect` got around to recv it.
            if let Some(rec) = recorder.as_mut()
                && let Err(err) = rec.record(&buf[..n])
            {
                eprintln!("pty-harness: cast recording stopped: {err}");
                recorder = None;
            }
            if tx.send(buf[..n].to_vec()).is_err() {
                // Receiver gone (scenario killed) — stop pumping, but still
                // fall through to flush the cast's carried bytes below.
                break;
            }
        }
        if let Some(rec) = recorder.as_mut() {
            rec.finish().ok();
        }
    });
    rx
}
