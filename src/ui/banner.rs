//! The shell's launch splash: a horned ox beside the AUROX lettering,
//! set in figlet's slant font — the italic of ASCII art. Uppercase on
//! purpose: slant's lowercase `a` and `o` bowls are near-identical, and a
//! banner that reads "ourox" defeats itself.

use super::grid::{Paint, Table};
use crate::context;
use console::{Term, style};
use std::io::{IsTerminal, Write};
use std::sync::mpsc;
use std::time::Duration;

/// The splash art, one row per line: the fixed-width ox-head column and the
/// lettering it flanks, kept as separate columns so each side takes its own
/// color without re-splitting rendered strings. The head fills the bottom
/// three rows, standing on the lettering's baseline.
const ART: &[(&str, &str)] = &[
    ("        ", "    ___   __  ______  ____ _  __"),
    ("        ", r"   /   | / / / / __ \/ __ \ |/ /"),
    ("  ^__^  ", "  / /| |/ / / / /_/ / / / /   /"),
    ("  (oo)  ", " / ___ / /_/ / _, _/ /_/ /   |"),
    ("  (__)  ", r"/_/  |_\____/_/ |_|\____/_/|_|"),
];

/// The crate version as the splash tags it (`v0.2.0`).
const VERSION_TAG: &str = concat!("v", env!("CARGO_PKG_VERSION"));

/// Row of [`ART`] carrying the ox's eyes (`(oo)`), and the glyphs the launch
/// blink swaps between. Named so the blink derives the eye *column* from the
/// art (never a hand-counted offset) and a test can lock them against the art
/// drifting under them.
const EYE_ROW: usize = 3;
const OPEN_EYES: &str = "oo";
const CLOSED_EYES: &str = "--";

/// Render the launch splash — the ox in yellow, the lettering in pacman's
/// bold headline blue, the version tag dim at the end of the last row.
///
/// The shell shows this once per session behind the `banner` config knob,
/// after the first-launch question (art must never bury a prompt). `Paint`
/// decides color exactly like every other renderer, so `--color never` —
/// and tests, which pin [`Paint::Plain`] — get the plain bytes.
pub fn launch_banner(paint: Paint) -> Table {
    let mut out = Table::new();
    for (i, (ox, letters)) in ART.iter().enumerate() {
        let mut line = if paint.colored() {
            format!("{}{}", style(*ox).yellow(), style(*letters).bold().blue())
        } else {
            format!("{ox}{letters}")
        };
        if i == ART.len() - 1 {
            line.push_str("  ");
            if paint.colored() {
                line.push_str(&super::dim(VERSION_TAG).to_string());
            } else {
                line.push_str(VERSION_TAG);
            }
        }
        out.push(line);
    }
    out
}

/// Eyes-shut time per unit — a *dit* blink; a *dah* is three of these. Kept
/// well under [`GAP_UNIT`] so each signal reads as a quick blink rather than the
/// eyes being held closed.
const BLINK_UNIT: Duration = Duration::from_millis(150);

/// Eyes-open time per rest unit: standard Morse spacing of 1 (between a letter's
/// own symbols) / 3 (between letters) / 7 (between repeats). Well over
/// [`BLINK_UNIT`] so the winks are set off by long pauses.
const GAP_UNIT: Duration = Duration::from_millis(900);

/// The rest between repeats of the call sign, in [`GAP_UNIT`]s (Morse's word
/// gap), spent with the eyes open.
const WORD_GAP: u32 = 7;

/// How long the prompt must sit untouched before the eyes start signalling —
/// long enough that a user who's about to type has struck their first key
/// (which cancels the blink) rather than being interrupted by it.
const SPLASH_IDLE: Duration = Duration::from_secs(5);

/// What the eyes spell, over and over, while the prompt sits idle: "AUROX" in
/// Morse, one `.`-dit / `-`-dah string per letter. The eye timeline is derived
/// from these in [`morse_timeline`], so the letters are the single source of
/// truth and a test checks the derivation.
const CALL_SIGN: &[&str] = &[".-", "..-", ".-.", "---", "-..-"];

/// The narrowest terminal the blink runs in.
///
/// The splash and its caption lines all fit well inside 80 columns (a `repl`
/// test pins the captions under it); below that a line could wrap, throwing
/// off the relative cursor moves the blink steps by — so it stays a static
/// banner instead.
pub const SPLASH_MIN_COLS: u16 = 80;

/// 0-based column of the ox's first eye within a banner line, read from the art
/// so it tracks [`ART`] instead of a hand-counted offset.
fn eye_column() -> usize {
    ART[EYE_ROW]
        .0
        .find(OPEN_EYES)
        .expect("the eye row carries the open eyes")
}

/// The eyes' on-screen position and colour — all a blink frame needs to draw.
/// `Copy`, so the background blink worker owns its own snapshot instead of
/// sharing the session.
#[derive(Clone, Copy)]
struct Eyes {
    /// Rows from the prompt up to the eye row: the caption lines, plus the last
    /// banner row (below the eyes) and the eye row itself.
    up: u16,
    /// 0-based column of the first eye glyph, from [`eye_column`].
    eye_col: usize,
    paint: Paint,
}

impl Eyes {
    /// Blink the [`CALL_SIGN`] ("AUROX") in Morse, looping until `interrupted`
    /// reports the user has typed. `interrupted(d)` waits up to `d` and returns
    /// `true` the instant input arrives, so the loop stops promptly — even
    /// mid-letter. Best-effort throughout and always leaves the eyes open: a
    /// splash flourish must never fail the shell nor abandon the eyes shut.
    fn blink_morse(self, interrupted: impl Fn(Duration) -> bool) {
        let timeline = morse_timeline();
        'blink: loop {
            for &(shut, units) in &timeline {
                // Shut steps use the shorter blink unit; open rests the longer
                // gap unit, so the signals are quick winks amid deliberate pauses.
                let (glyphs, unit) = if shut {
                    (CLOSED_EYES, BLINK_UNIT)
                } else {
                    (OPEN_EYES, GAP_UNIT)
                };
                if write_stdout(&self.frame(glyphs)).is_err() || interrupted(unit * units) {
                    break 'blink;
                }
            }
            // The word gap (eyes open) before the call sign repeats.
            if write_stdout(&self.frame(OPEN_EYES)).is_err() || interrupted(GAP_UNIT * WORD_GAP) {
                break 'blink;
            }
        }
        // However the loop ended, leave the eyes open so the banner reads normally.
        write_stdout(&self.frame(OPEN_EYES)).ok();
    }

    /// The escape sequence that repaints just the eyes with `glyphs` and puts
    /// the cursor back where it was (the prompt): save · up to the eye row ·
    /// across to the eye column · the glyphs, yellow like the rest of the ox ·
    /// restore.
    fn frame(self, glyphs: &str) -> String {
        let glyphs = if self.paint.colored() {
            style(glyphs).yellow().to_string()
        } else {
            glyphs.to_owned()
        };
        // `\x1b7`/`\x1b8` = DEC save/restore cursor; `…A` = cursor up N rows;
        // `…G` = cursor to (1-based) column.
        format!(
            "\x1b7\x1b[{}A\x1b[{}G{glyphs}\x1b8",
            self.up,
            self.eye_col + 1
        )
    }
}

/// The launch splash's blinking ox eyes, wired to the line editor.
///
/// While the first prompt sits untouched, the eyes blink "AUROX" ([`CALL_SIGN`])
/// in Morse, looping until the user types. [`arm`](Self::arm) builds one only
/// for a terminal that can carry it; the shell hands
/// [`cancel_on_keystroke`](Self::cancel_on_keystroke) to the editor's helper and
/// runs the first read through [`run`](Self::run), which blinks in the
/// background and stops on the first keystroke (or when the read returns). On any
/// narrower or non-interactive session `arm` returns `None` and the banner stays
/// still — so the whole blink, thread and channel included, stays behind this
/// one type.
pub struct SplashBlink {
    eyes: Eyes,
    /// The blink's off switch: cloned to the editor's helper (fires on the first
    /// keystroke) and kept here (fires when the first read returns). Either send
    /// stops the worker.
    cancel: mpsc::Sender<()>,
    /// What the worker waits on — a `cancel` send, or a genuine idle timeout,
    /// drives it.
    idle: mpsc::Receiver<()>,
}

impl SplashBlink {
    /// Decide whether the splash may wink, and with what geometry.
    /// `caption_lines` is how many lines print between the banner and the
    /// prompt. Returns `None` — a still banner — unless stdout is a real
    /// terminal, colour is on, and the terminal is big enough that the whole
    /// splash is on-screen and unwrapped (so the blink's relative cursor moves
    /// land where the eyes actually are).
    pub fn arm(paint: Paint, caption_lines: usize) -> Option<Self> {
        if !paint.colored() || !std::io::stdout().is_terminal() {
            return None;
        }
        let (rows, cols) = Term::stdout().size();
        // Rows from the prompt up to the eyes: past the caption lines, then up
        // the banner from its baseline to the eye row — the eye row plus the
        // rows beneath it, i.e. `ART.len() - EYE_ROW`.
        let up = caption_lines + (ART.len() - EYE_ROW);
        // The banner, captions and the prompt line must all be on-screen (no
        // scroll) for that upward move to land on the eyes.
        let need_rows = ART.len() + caption_lines + 1; // + the prompt line
        if cols < SPLASH_MIN_COLS || (rows as usize) < need_rows {
            return None;
        }
        let (cancel, idle) = mpsc::channel();
        Some(Self {
            eyes: Eyes {
                up: u16::try_from(up).ok()?,
                eye_col: eye_column(),
                paint,
            },
            cancel,
            idle,
        })
    }

    /// The sender to hand the line editor's helper, so the first keystroke
    /// cancels the blink before it can land on the line being typed.
    pub fn cancel_on_keystroke(&self) -> mpsc::Sender<()> {
        self.cancel.clone()
    }

    /// Run `read_line` (the first prompt's read) with the eyes blinking in the
    /// background, and return whatever it returns. The blink starts only after
    /// [`SPLASH_IDLE`] of quiet and stops the instant a keystroke arrives (via
    /// the helper) or `read_line` returns — whichever comes first.
    pub fn run<T>(self, read_line: impl FnOnce() -> T) -> T {
        let Self { eyes, cancel, idle } = self;
        context::scope(|s| {
            s.spawn(move || {
                // `interrupted(d)`: wait up to `d`; true the instant a keystroke
                // or the read-return arrives on `cancel` (anything but a genuine
                // timeout). It's both the idle-window gate and, inside
                // `blink_morse`, the interruptible rest between symbols.
                let interrupted =
                    |d| !matches!(idle.recv_timeout(d), Err(mpsc::RecvTimeoutError::Timeout));
                if !interrupted(SPLASH_IDLE) {
                    eyes.blink_morse(interrupted);
                }
            });
            let out = read_line();
            cancel.send(()).ok(); // stop the worker if it's still blinking
            out
        })
    }
}

/// Write `s` to stdout under a fresh lock and flush — the lock isn't held
/// across the inter-symbol rests, so a keystroke racing the blink isn't stalled
/// behind it.
fn write_stdout(s: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(s.as_bytes())?;
    out.flush()
}

/// The eye timeline for [`CALL_SIGN`]: `(eyes_shut, units)` steps. A dit is one
/// unit shut, a dah three, with one unit open between a letter's own symbols and
/// three between letters — standard Morse spacing. Derived from the letters so
/// the glyphs and their timing can't drift apart.
fn morse_timeline() -> Vec<(bool, u32)> {
    let mut steps = Vec::new();
    for (i, letter) in CALL_SIGN.iter().enumerate() {
        for (j, symbol) in letter.chars().enumerate() {
            if j > 0 {
                steps.push((false, 1)); // rest between a letter's own symbols
            }
            steps.push((true, if symbol == '-' { 3 } else { 1 }));
        }
        if i + 1 < CALL_SIGN.len() {
            steps.push((false, 3)); // rest between letters
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::assert_contains;

    /// The plain splash: one line per art row with no ANSI escapes, the ox
    /// beside the lettering, and the crate version tagged onto the last row.
    #[test]
    fn plain_splash_shape() {
        let table = launch_banner(Paint::Plain);
        let lines = table.lines();
        assert_eq!(lines.len(), ART.len(), "one line per art row: {lines:?}");
        for line in lines {
            assert!(
                !line.contains('\u{1b}'),
                "plain must carry no ANSI: {line:?}"
            );
        }
        assert_contains!(lines[3], "(oo)");
        assert_contains!(lines[4], VERSION_TAG);
    }

    /// The colored splash carries ANSI styling on every row and strips back
    /// to the exact plain bytes, so the two paints can't drift apart.
    #[test]
    fn colored_splash_strips_to_plain() {
        // `console` gates styling on its own stdout-TTY detection at render
        // time; force it on so the colored branch is observable when the test
        // runs piped (plain `cargo test`), not only under makepkg's tty.
        console::set_colors_enabled(true);
        let plain = launch_banner(Paint::Plain);
        let colored = launch_banner(Paint::Colored);
        for (c, p) in colored.lines().iter().zip(plain.lines()) {
            assert_contains!(c, "\u{1b}[");
            assert_eq!(
                console::strip_ansi_codes(c),
                *p,
                "colored row must strip to the plain bytes"
            );
        }
    }

    /// The blink reads the eye position out of the art, so any drift in the
    /// eyes (moved, re-glyphed, resized) trips here before it can mis-position
    /// the wink at a fixed offset.
    #[test]
    fn eye_glyphs_track_the_art() {
        assert_contains!(ART[EYE_ROW].0, OPEN_EYES);
        assert_eq!(
            OPEN_EYES.len(),
            CLOSED_EYES.len(),
            "open and shut frames must be the same width"
        );
        assert_eq!(eye_column(), 3, "`  (oo)  ` puts the eyes at column 3");
    }

    /// A plain frame is pure cursor choreography — save, up to the eye row,
    /// across to the eye column, the glyphs, restore — and the open frame
    /// differs only in the glyphs, so the wink round-trips the eyes back.
    #[test]
    fn blink_frame_repositions_and_restores() {
        let eyes = Eyes {
            up: 4,
            eye_col: eye_column(),
            paint: Paint::Plain,
        };
        assert_eq!(eyes.frame(CLOSED_EYES), "\x1b7\x1b[4A\x1b[4G--\x1b8");
        assert_eq!(eyes.frame(OPEN_EYES), "\x1b7\x1b[4A\x1b[4Goo\x1b8");
    }

    /// The winking eyes carry the ox's yellow, wrapped in the same save · up ·
    /// across · restore choreography as the plain frame.
    #[test]
    fn blink_frame_paints_the_eyes() {
        console::set_colors_enabled(true);
        let eyes = Eyes {
            up: 3,
            eye_col: eye_column(),
            paint: Paint::Colored,
        };
        let frame = eyes.frame(CLOSED_EYES);
        assert_contains!(frame, "\u{1b}[33m"); // yellow SGR on the eyes
        assert!(
            frame.starts_with("\x1b7\x1b[3A\x1b[4G"),
            "saves the cursor, steps up to the eye row and column: {frame:?}"
        );
        assert!(
            frame.ends_with("\x1b8"),
            "restores the cursor to the prompt: {frame:?}"
        );
    }

    /// The eye timeline is real Morse for the call sign: every step toggles the
    /// eyes (no dead no-op frames), the shut steps are exactly the dits and dahs
    /// of every letter, and "A" (dit-dah) opens the sign.
    #[test]
    fn morse_timeline_spells_the_call_sign() {
        let timeline = morse_timeline();
        assert!(
            timeline.windows(2).all(|w| w[0].0 != w[1].0),
            "each step must flip the eyes: {timeline:?}"
        );
        let symbols: usize = CALL_SIGN.iter().map(|l| l.chars().count()).sum();
        assert_eq!(
            timeline.iter().filter(|(shut, _)| *shut).count(),
            symbols,
            "one shut step per dit/dah across the call sign"
        );
        // 'A' = dit dah: shut 1, open 1, shut 3, then the 3-unit letter gap.
        assert_eq!(
            &timeline[..4],
            &[(true, 1), (false, 1), (true, 3), (false, 3)]
        );
        // Dits are one unit shut, dahs three — nothing else.
        assert!(
            timeline
                .iter()
                .filter(|(shut, _)| *shut)
                .all(|&(_, u)| u == 1 || u == 3)
        );
    }
}
