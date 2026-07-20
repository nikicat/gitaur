//! The REPL entry: the first-launch question, the pre-prompt banner, and
//! [`run`]'s rustyline read-dispatch loop — the only code in the shell that
//! touches the line editor.

use super::command::{self, Command};
use super::complete::ShellHelper;
use super::env::{RealEnv, build_universe, cart_targets};
use super::{Flow, ShellEnv, State};
use crate::build::DevelPolicy;
use crate::config::ConfigHandle;
use crate::error::{Error, Result};
use crate::index::{self, AurIndexData};
use crate::mirror;
use crate::names::SearchTerm;
use crate::paths;
use crate::ui;
use rustyline::error::ReadlineError;
use rustyline::history::DefaultHistory;
use rustyline::{ColorMode as RlColorMode, Config as RlConfig, Editor};
use signal_hook::consts::SIGINT;
use std::rc::Rc;
use tracing::{debug, info, instrument};

/// Exit code for the Ctrl-C quit: the shell convention `128 + signal number`
/// for SIGINT, derived from the same constant the signal handlers use rather
/// than a re-typed 130. Scripts driving the shell can tell this interrupt
/// quit from `quit`/Ctrl-D's 0.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const CTRL_C_EXIT_CODE: u8 = 128 + SIGINT as u8;

/// The pre-prompt banner: what this session covers. Pure so the wording is
/// testable. Runs *after* the first-launch question, so `NotSetUp` here means
/// the user chose "later" — one reminder line, not a re-pitch (the question
/// already spelled out the cost). Pacman-only mode gets a marker instead of a
/// nag: `aur = false` is a standing choice, not a missing step.
fn startup_lines(aur: index::AurState) -> Vec<&'static str> {
    match aur {
        index::AurState::Ready => {
            vec!["aurox shell — type `help` for commands, `quit` to leave"]
        }
        index::AurState::NotSetUp => vec![
            "aurox shell — type `help` for commands, `quit` to leave",
            "pacman-only this session — `refresh aur` syncs the AUR anytime",
        ],
        index::AurState::Disabled => {
            vec!["aurox shell (pacman-only) — type `help` for commands, `quit` to leave"]
        }
    }
}

/// Whether the launch splash — the ox art and its idle eye-blink — shows.
/// Only a bare `aurox` launch qualifies: the `banner` knob must be on (default)
/// *and* there must be no seeded search. `aurox <term>` seeds a `search` whose
/// numbered result table prints between the art and the prompt — it would bury
/// the art, and (since the blink steps a fixed number of rows up to the eyes)
/// stamp any wink onto the wrong row (or off a scrolled banner) — so a seeded
/// launch goes straight to its results with no splash. Every explicit op
/// (`-Su`, `-Ss`, …) never enters the shell at all. Pure so the rule is
/// unit-tested.
const fn splash_shown(banner_enabled: bool, seeded_search: bool) -> bool {
    banner_enabled && !seeded_search
}

/// The shell's first-launch question, asked while the AUR is enabled but was
/// never synced: sync now / pacman-only from now on / later.
///
/// Persistence is minimal by construction — "yes" persists as the mirror +
/// index artifact itself, "no" as an `aur = false` line written through
/// [`ConfigHandle::update`] (the one place aurox edits its own config, which
/// also flips the in-memory view so the rest of the session sees the
/// choice), "later" as nothing at all (asked again next launch).
fn first_launch_setup(mut config: ConfigHandle) -> Result<ConfigHandle> {
    if index::AurState::probe(config.cfg()) != index::AurState::NotSetUp {
        return Ok(config);
    }
    match ui::aur_setup_prompt().map_err(|e| Error::other(format!("setup prompt: {e}")))? {
        ui::AurSetupChoice::SyncNow => {
            // Consent was just given — ShellAurSync runs the bootstrap
            // without a second question.
            mirror::cmd_refresh(
                config.cfg(),
                mirror::RefreshReason::ShellAurSync,
                mirror::RefreshScope::Everything,
            )?;
        }
        ui::AurSetupChoice::PacmanOnly => {
            config.update(|c| c.aur = Some(false))?;
            ui::note(&format!(
                "pacman-only mode saved (`aur = false` in {}) — delete the line and `refresh aur` to opt back in",
                config.path().display()
            ));
        }
        ui::AurSetupChoice::Later => {}
    }
    Ok(config)
}

/// Run the interactive shell. Returns the desired process exit code.
///
/// `initial_search` seeds the session: when launched via the bare-positional
/// shortcut (`aurox <term>…`), dispatch passes the typed terms here and the shell
/// runs one `search` before the prompt loop — identical to starting the shell
/// and typing `search <term>…`. Empty for the plain no-arg `aurox` launch.
#[instrument(skip(config))]
pub fn run(config: &ConfigHandle, devel: DevelPolicy, initial_search: &[SearchTerm]) -> Result<u8> {
    info!(devel = ?devel, terms = initial_search.len(), "shell session start");
    // First-launch question (no-op unless the AUR is enabled-but-unsynced).
    // Owns a local handle so a "pacman-only" answer takes effect immediately.
    let config = first_launch_setup(config.clone())?;
    let cfg = config.cfg();
    // Once per session: load the AUR index (+ lookup maps) and the name
    // universe. Not repeated per command; `refresh` (later phase) re-fetches.
    // The AUR data loads empty (not absent) when the AUR isn't in play.
    let aur_state = index::AurState::probe(cfg);
    let aur_data = AurIndexData::load(cfg)?;
    let caches = build_universe(&aur_data);
    debug!(
        names = caches.universe.len(),
        sync = caches.sync.len(),
        aur = ?aur_state,
        "shell session loaded"
    );
    let mut env = RealEnv {
        cfg,
        devel,
        aur_data,
        aur_state,
        caches,
    };
    let mut state = State::default();

    // The splash — ox art, behind the `banner` knob (default on) — but only on a
    // bare `aurox` launch: a seeded `aurox <term>` goes straight to its results
    // (see `splash_shown`). After the first-launch question — art must never bury
    // a prompt — and before the caption, so the one-liner reads as the art's
    // caption whenever the art is there.
    let paint = ui::Paint::detect();
    let banner_shown = splash_shown(cfg.banner, !initial_search.is_empty());
    if banner_shown {
        env.print_table(&ui::launch_banner(paint));
    }
    let captions = startup_lines(aur_state);
    for line in captions.iter().copied() {
        env.print(line);
    }
    // Arm the splash's idle eye-blink, gated further on the terminal by `arm`.
    // `mut` so the first prompt can `take` it. No banner ⇒ no eyes to blink.
    let mut blink = banner_shown
        .then(|| ui::SplashBlink::arm(paint, captions.len()))
        .flatten();

    // Seed the session with the launch-time search (`aurox <term>…`): run it once
    // up front so the numbered result list is on screen before the first prompt,
    // exactly as if the user had typed `search <term>…`.
    if !initial_search.is_empty() {
        state.dispatch(&Command::Search(initial_search.to_vec()), &mut env);
    }

    let helper = ShellHelper::new(Rc::clone(&env.caches.universe));
    // Follow the session's colour mode so `--color never` also stops rustyline
    // from dimming the history hint (it skips `highlight_hint` when Disabled).
    let rl_config = RlConfig::builder()
        .color_mode(match cfg.color_mode() {
            ui::ColorMode::Always => RlColorMode::Forced,
            ui::ColorMode::Never => RlColorMode::Disabled,
            ui::ColorMode::Auto => RlColorMode::Enabled,
        })
        .build();
    let mut rl: Editor<ShellHelper, DefaultHistory> = Editor::with_config(rl_config)
        .map_err(|e| Error::other(format!("shell: init line editor: {e}")))?;
    rl.set_helper(Some(helper));
    let history = paths::shell_history_path();
    // A missing history file on first run is expected, not an error.
    rl.load_history(&history).ok();

    // Hand the blink its off switch: the helper fires it on the first keystroke
    // so the wink never lands on a line in progress. The blink itself — thread,
    // channel and timing — lives behind `SplashBlink::run`.
    if let Some(blink) = &blink
        && let Some(helper) = rl.helper_mut()
    {
        helper.watch_first_keystroke(blink.cancel_on_keystroke());
    }

    let code = loop {
        // The prompt is recomputed per line: it carries the cart's standing
        // (counts + open review gates), so state stays ambient at the prompt
        // instead of being reprinted after every command.
        let prompt = state.prompt();
        // The first prompt runs the read with the eyes blinking behind it; every
        // prompt after is a plain read.
        let readline = match blink.take() {
            Some(blink) => blink.run(|| rl.readline(&prompt)),
            None => rl.readline(&prompt),
        };
        match readline {
            Ok(line) => {
                if !line.trim().is_empty() {
                    // Best-effort: a full history ring shouldn't abort input.
                    rl.add_history_entry(line.as_str()).ok();
                }
                let flow = state.dispatch(&command::parse(&line), &mut env);
                // Refresh Tab's view for the next line: the just-mutated cart,
                // and the universe (a cheap `Rc` clone — only `upgrade`/`refresh`
                // actually swaps it). Sharing the same sources the selector
                // resolver uses keeps "what Tab offers" == "what the verb accepts".
                if let Some(helper) = rl.helper_mut() {
                    helper.sync(Rc::clone(&env.caches.universe), cart_targets(&state));
                }
                if let Flow::Exit(code) = flow {
                    break code;
                }
            }
            // Ctrl-C at the prompt leaves the shell, like Ctrl-D — during a
            // long operation it aborts back to the prompt instead (each op
            // holds its own SIGINT guard), so quitting is what an *idle* ^C
            // can still usefully mean.
            Err(ReadlineError::Interrupted) => break CTRL_C_EXIT_CODE,
            // Ctrl-D at the prompt exits cleanly.
            Err(ReadlineError::Eof) => break 0,
            Err(e) => return Err(Error::other(format!("shell: read line: {e}"))),
        }
    };

    // History persistence is best-effort: a save failure shouldn't fail the run.
    if let Err(e) = rl.save_history(&history) {
        debug!(error = %e, "shell: could not save history");
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::assert_contains;

    /// The derived Ctrl-C quit code is the exact value the docs, the e2e
    /// drivers, and any wrapper script rely on — an external contract, so
    /// the concrete number is the assertion.
    #[test]
    fn ctrl_c_exit_code_is_the_shell_convention() {
        assert_eq!(CTRL_C_EXIT_CODE, 130);
    }

    /// The pre-prompt banner: a ready session gets the one-liner, a "later"
    /// answer gets one reminder line (the launch question already pitched
    /// the cost), and pacman-only mode is marked instead of nagged.
    #[test]
    fn startup_banner_variants() {
        let ready = startup_lines(index::AurState::Ready);
        assert_eq!(ready.len(), 1, "ready session banners one line: {ready:?}");

        let later = startup_lines(index::AurState::NotSetUp);
        assert_eq!(
            later.len(),
            2,
            "one reminder line, not a re-pitch: {later:?}"
        );
        assert_contains!(later[1], "`refresh aur`");

        let pacman_only = startup_lines(index::AurState::Disabled);
        assert_eq!(
            pacman_only.len(),
            1,
            "pacman-only mode must not nag: {pacman_only:?}"
        );
        assert_contains!(pacman_only[0], "(pacman-only)");
    }

    /// The splash blink steps by relative cursor moves, which only land while
    /// nothing has wrapped; [`ui::SplashBlink::arm`] guarantees that by
    /// requiring [`ui::SPLASH_MIN_COLS`] columns, so every caption must fit
    /// inside that floor — this catches a future caption that grows past it.
    #[test]
    fn startup_captions_fit_the_blink_width() {
        for aur in [
            index::AurState::Ready,
            index::AurState::NotSetUp,
            index::AurState::Disabled,
        ] {
            for line in startup_lines(aur) {
                assert!(
                    line.chars().count() < ui::SPLASH_MIN_COLS as usize,
                    "caption wider than the blink's width floor: {line:?}"
                );
            }
        }
    }

    /// The splash shows only on a bare `aurox` launch with the banner knob on.
    /// `aurox <term>` seeds a search whose result table would print between the
    /// art and the prompt (burying the art and the fixed-row eye-blink), so a
    /// seeded launch shows no splash — it goes straight to its results. A
    /// disabled banner never shows it either way.
    #[test]
    fn splash_shows_only_on_a_bare_launch() {
        let (seeded, bare) = (true, false);
        assert!(
            splash_shown(true, bare),
            "bare launch with the knob on: the ox shows, then the prompt"
        );
        assert!(
            !splash_shown(true, seeded),
            "a seeded search goes straight to its results — no art to bury"
        );
        assert!(
            !splash_shown(false, bare),
            "the banner knob off means no splash"
        );
        assert!(!splash_shown(false, seeded));
    }
}
