//! Consent gate for the AUR bootstrap clone.
//!
//! The first AUR touch needs a full clone of the AUR monorepo — a ~2 GiB
//! download that takes ~10 minutes — and `-Syy` re-downloads it on purpose.
//! Neither ever starts silently: every path that could trigger a bootstrap
//! funnels through [`plan`], which announces the cost and asks first.
//! Incremental fetches of an existing mirror never prompt, and `aur = false`
//! in config.toml (pacman-only mode) skips the AUR half — prompts included.
//!
//! The gate is two pure decisions run in sequence. First, what the AUR half
//! *wants* ([`decide`]):
//!
//! | `aur` cfg | mirror on disk | trigger         | wants                          |
//! |-----------|----------------|-----------------|--------------------------------|
//! | off       | any            | any             | skip — pacman-only, no prompt  |
//! | on        | ready          | `-Syy`          | bootstrap (forced re-clone)    |
//! | on        | ready          | anything else   | incremental fetch, no prompt   |
//! | on        | interrupted    | any             | bootstrap (redo from scratch)  |
//! | on        | absent         | any             | bootstrap (first run)          |
//!
//! Second, how a wanted bootstrap obtains consent ([`consent_mode`]), by
//! trigger (`--noconfirm` short-circuits every row to auto-yes — except the
//! install offer, which it refuses):
//!
//! | trigger                     | stdin a TTY | consent                                    |
//! |-----------------------------|-------------|--------------------------------------------|
//! | `-Sy` / `-Syy`              | yes         | announce + Y/n prompt (default yes)        |
//! | `-Sy` / `-Syy`              | no          | announce + read line: EOF ⇒ yes, `n` ⇒ no  |
//! | shell `refresh aur` / launch "yes" | (tty) | auto-yes — asking for the AUR by name IS the consent |
//! | shell `refresh` (bare)      | (tty)       | refuse quietly (`SkipCause::NotSetUp`)     |
//! | shell `upgrade`             | (tty)       | refuse quietly (`SkipCause::NotSetUp`)     |
//! | schema-bump resync (implicit) | yes       | announce + Y/n prompt (default yes)        |
//! | schema-bump resync (implicit) | no        | refuse — never bootstrap behind a pipe     |
//! | `-S` unknown-target offer   | yes         | announce + Y/n prompt (default yes)        |
//! | `-S` unknown-target offer   | no / `--noconfirm` | refuse — an offer is never auto-accepted |
//!
//! The shell rows exist because the shell asks its own three-way question at
//! launch (sync now / pacman-only / later): after "later", the session is
//! pacman-only until the user names the AUR — `refresh aur` IS the consent,
//! no second Y/n — while the bare `refresh` and `upgrade`'s TTL-driven fetch
//! must not spring the clone on someone who just said "later".
//!
//! A decline or refusal still refreshes the official sync DBs, still records
//! the fetch-TTL stamp (so a TTL-driven `upgrade` doesn't re-fetch within
//! the window), and surfaces as [`RefreshOutcome::AurSkipped`] with its
//! [`SkipCause`] so every caller can word what was skipped.

use crate::config::Config;
use crate::error::Result;
use crate::paths;
use crate::runopts;
use crate::ui;
use std::fmt;
use std::io::IsTerminal;
use std::path::Path;
use tracing::info;

/// Who asked for this refresh — picks the `-Syy` force-reclone behaviour and
/// how consent for a needed bootstrap is obtained (see [`consent_mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshReason {
    /// `aurox -Sy` — explicit CLI refresh.
    ExplicitSync,
    /// `aurox -Syy` — explicit forced re-clone (wipes the mirror first).
    ForceReclone,
    /// Shell's explicit AUR sync — `refresh aur`, or a "yes" to the
    /// first-launch question. Pre-consented: the launch prompt (or the
    /// startup hint) already spelled out the bootstrap cost, and naming the
    /// AUR after that IS the answer — no second Y/n.
    ShellAurSync,
    /// Shell's bare `refresh` — refreshes what the session already has and
    /// must never spring a bootstrap on a user who chose "later" at launch:
    /// skip the AUR half quietly and let the caller point at `refresh aur`.
    ShellRefresh,
    /// Shell `upgrade` — its TTL-driven fetch must never spring a bootstrap
    /// on a user who chose "later" at launch: skip the AUR half quietly and
    /// let the caller hint instead.
    ShellUpgrade,
    /// [`crate::index::load_or_resync`]'s schema-bump rebuild — implicit: the
    /// user typed something unrelated (`-Ss`, `-S`, …), so a non-interactive
    /// run must never bootstrap on its behalf.
    IndexResync,
    /// `aurox -S <target>` hit unknown targets with the AUR
    /// enabled-but-unsynced — the names may simply live there, so the install
    /// path offers the one-time setup and retries. An *offer*, not a command:
    /// it prompts on a TTY and is refused everywhere else — `--noconfirm`
    /// included, since `-S --noconfirm <typo>` in a script must not pull a
    /// surprise ~2 GiB clone.
    InstallOffer,
}

/// What one [`super::cmd_refresh`] actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// AUR mirror + index are current (bootstrap, incremental, or a no-op fetch).
    Refreshed,
    /// The AUR half was skipped; the official sync DBs were still refreshed.
    AurSkipped(SkipCause),
}

/// Why the AUR half of a refresh was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipCause {
    /// `aur = false` in config.toml — pacman-only mode.
    Disabled,
    /// The user answered "n" to the bootstrap prompt.
    Declined,
    /// The AUR was never synced and this trigger (bare `refresh`, `upgrade`)
    /// doesn't prompt — the user's launch-time "later" stands until they say
    /// `refresh aur`.
    NotSetUp,
    /// An implicit trigger needed a bootstrap but had no terminal to ask on.
    NonInteractive,
    /// The command's [`RefreshScope`](super::RefreshScope) excluded the AUR
    /// half (`refresh pacman`) — nothing to consent to, nothing to report.
    NotRequested,
}

impl fmt::Display for SkipCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Disabled => "aur = false in config",
            Self::Declined => "declined",
            Self::NotSetUp => "AUR not set up",
            Self::NonInteractive => "non-interactive run",
            Self::NotRequested => "not requested",
        })
    }
}

/// The resolved fate of the AUR half of one refresh. [`decide`] produces it
/// with `Bootstrap` meaning "wants a bootstrap"; [`plan`] applies the consent
/// step, so a `Bootstrap` returned from there is already approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AurAction {
    /// Incremental fetch of the existing mirror.
    Fetch,
    /// Full clone + index rebuild from scratch.
    Bootstrap(BootstrapKind),
    /// Leave the AUR mirror alone (the repo-db sync still runs).
    Skip(SkipCause),
}

/// Which flavour of full clone is about to run — picks the announcement copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BootstrapKind {
    /// No mirror on disk yet.
    FirstRun,
    /// A previous bootstrap died before writing refs; redo from scratch.
    InterruptedRedo,
    /// `-Syy`: a healthy mirror is deliberately re-cloned.
    ForcedReclone,
}

/// On-disk state of the mirror, per [`super::is_bootstrapped`]'s artifact rule
/// (refs exist ⇔ the clone finished).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MirrorState {
    /// Bootstrapped and usable — refreshes are incremental.
    Ready,
    /// A directory exists but has no branches: an interrupted clone.
    Interrupted,
    /// Nothing on disk.
    Absent,
}

impl MirrorState {
    fn probe(path: &Path) -> Self {
        if !path.exists() {
            Self::Absent
        } else if super::is_bootstrapped(path) {
            Self::Ready
        } else {
            Self::Interrupted
        }
    }
}

/// How consent for a needed bootstrap is obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConsentMode {
    /// Consent already given — `--noconfirm`, or a shell `refresh` typed
    /// after the launch prompt spelled out the cost. Announce and proceed
    /// (so logs show what was agreed to).
    AutoYes,
    /// Ask via [`ui::confirm`] — dialoguer on a TTY; on a pipe the read-line
    /// fallback, where EOF takes the yes default and a piped `n` declines
    /// (`-Sy` in a script is itself the explicit ask, and cron/CI runs with a
    /// closed stdin keep working).
    Prompt,
    /// Never bootstrap on this trigger: skip with the reason's [`SkipCause`].
    Refuse,
}

/// Pure consent-resolution core, kept parameter-injected for the unit tests
/// ([`plan`] feeds it the live `--noconfirm` flag and stdin's TTY-ness).
const fn consent_mode(reason: RefreshReason, noconfirm: bool, stdin_is_tty: bool) -> ConsentMode {
    match reason {
        // The one trigger `--noconfirm` refuses instead of auto-accepting:
        // it's an offer aurox volunteered, not something the user asked for.
        RefreshReason::InstallOffer => {
            if stdin_is_tty && !noconfirm {
                ConsentMode::Prompt
            } else {
                ConsentMode::Refuse
            }
        }
        _ if noconfirm => ConsentMode::AutoYes,
        RefreshReason::ExplicitSync | RefreshReason::ForceReclone => ConsentMode::Prompt,
        RefreshReason::ShellAurSync => ConsentMode::AutoYes,
        RefreshReason::ShellRefresh | RefreshReason::ShellUpgrade => ConsentMode::Refuse,
        RefreshReason::IndexResync => {
            if stdin_is_tty {
                ConsentMode::Prompt
            } else {
                ConsentMode::Refuse
            }
        }
    }
}

/// What a [`ConsentMode::Refuse`] skip reports: the bare `refresh` and
/// `upgrade` skip because the AUR isn't set up yet; the implicit resync — and
/// a refused install offer — because there was nobody to ask (the install
/// path pre-gates on a TTY and falls back to its plain unknown-target error
/// either way).
const fn refusal_cause(reason: RefreshReason) -> SkipCause {
    match reason {
        RefreshReason::ShellRefresh | RefreshReason::ShellUpgrade => SkipCause::NotSetUp,
        _ => SkipCause::NonInteractive,
    }
}

/// Pure decision core: what the AUR half wants to do, before consent.
const fn decide(aur_enabled: bool, state: MirrorState, reason: RefreshReason) -> AurAction {
    if !aur_enabled {
        return AurAction::Skip(SkipCause::Disabled);
    }
    match (reason, state) {
        (RefreshReason::ForceReclone, MirrorState::Ready) => {
            AurAction::Bootstrap(BootstrapKind::ForcedReclone)
        }
        (_, MirrorState::Ready) => AurAction::Fetch,
        (_, MirrorState::Interrupted) => AurAction::Bootstrap(BootstrapKind::InterruptedRedo),
        (_, MirrorState::Absent) => AurAction::Bootstrap(BootstrapKind::FirstRun),
    }
}

/// Resolve what this refresh does to the AUR mirror: the pure [`decide`] step,
/// then — when a bootstrap is wanted — the cost announcement and the consent
/// prompt. Must run before the progress display exists (a prompt under live
/// indicatif rows gets clobbered by redraws).
pub(super) fn plan(cfg: &Config, reason: RefreshReason) -> Result<AurAction> {
    let state = MirrorState::probe(&paths::aur_repo_path());
    let mut action = decide(cfg.aur, state, reason);
    match action {
        AurAction::Bootstrap(kind) => action = bootstrap_consent(kind, reason)?,
        // Explicit CLI syncs get a one-line note; the shell words its own
        // outcome and the implicit resync surfaces the cause in its error.
        AurAction::Skip(SkipCause::Disabled)
            if matches!(
                reason,
                RefreshReason::ExplicitSync | RefreshReason::ForceReclone
            ) =>
        {
            ui::note(
                "AUR disabled (aur = false in config.toml); refreshing official package databases only",
            );
        }
        _ => {}
    }
    info!(reason = ?reason, state = ?state, action = ?action, "aur refresh plan");
    Ok(action)
}

/// Put a wanted bootstrap through the consent gate: announce the cost, then
/// confirm/auto-accept/refuse per [`consent_mode`], downgrading to a
/// [`AurAction::Skip`] when consent isn't given. One flat match — the
/// module's decision-fn convention — instead of a nested reassignment inside
/// [`plan`]'s own match.
fn bootstrap_consent(kind: BootstrapKind, reason: RefreshReason) -> Result<AurAction> {
    let mode = consent_mode(reason, runopts::noconfirm(), std::io::stdin().is_terminal());
    Ok(match mode {
        // The shell's launch prompt already spelled out the full cost this
        // session; `refresh aur` just gets a brief heads-up that the long
        // clone is starting.
        ConsentMode::AutoYes
            if reason == RefreshReason::ShellAurSync && kind == BootstrapKind::FirstRun =>
        {
            ui::info("syncing the AUR — one-time ~2 GiB clone (~10 min)");
            AurAction::Bootstrap(kind)
        }
        ConsentMode::AutoYes => {
            announce(kind);
            AurAction::Bootstrap(kind)
        }
        ConsentMode::Prompt => {
            announce(kind);
            if ui::confirm(question(kind), false)? {
                AurAction::Bootstrap(kind)
            } else {
                AurAction::Skip(SkipCause::Declined)
            }
        }
        ConsentMode::Refuse => AurAction::Skip(refusal_cause(reason)),
    })
}

/// Print the cost announcement for the flavour of clone about to be proposed.
fn announce(kind: BootstrapKind) {
    match kind {
        BootstrapKind::FirstRun => {
            ui::info("first-time AUR setup — aurox mirrors the whole AUR as one git repo");
            ui::note(
                "~2 GiB download, ~2.5 GiB on disk, ~10 min — one-time; refreshes afterwards are small incremental fetches",
            );
            ui::note("enables AUR search, info, install, and upgrades");
            ui::note(&format!(
                "pacman-only instead? set `aur = false` in {}",
                paths::config_path().display()
            ));
        }
        BootstrapKind::InterruptedRedo => {
            ui::warn("previous bootstrap was interrupted; the clone must restart from scratch");
            ui::note("~2 GiB download, ~10 min");
        }
        BootstrapKind::ForcedReclone => {
            ui::info("-Syy re-clones the AUR mirror from scratch: ~2 GiB download, ~10 min");
        }
    }
}

/// The Y/n question matching [`announce`]'s copy.
const fn question(kind: BootstrapKind) -> &'static str {
    match kind {
        BootstrapKind::FirstRun | BootstrapKind::InterruptedRedo => "clone the AUR mirror now?",
        BootstrapKind::ForcedReclone => "delete the existing mirror and re-clone?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_REASONS: [RefreshReason; 7] = [
        RefreshReason::ExplicitSync,
        RefreshReason::ForceReclone,
        RefreshReason::ShellAurSync,
        RefreshReason::ShellRefresh,
        RefreshReason::ShellUpgrade,
        RefreshReason::IndexResync,
        RefreshReason::InstallOffer,
    ];

    /// `--noconfirm` is the automation opt-in: it approves a bootstrap for any
    /// trigger the user actually issued, terminal or not. The install offer is
    /// the exception — aurox volunteered it, so `--noconfirm` refuses.
    #[test]
    fn noconfirm_auto_approves_every_reason_except_the_offer() {
        for reason in ALL_REASONS {
            let expected = if reason == RefreshReason::InstallOffer {
                ConsentMode::Refuse
            } else {
                ConsentMode::AutoYes
            };
            for tty in [true, false] {
                assert_eq!(
                    consent_mode(reason, true, tty),
                    expected,
                    "{reason:?} tty={tty}"
                );
            }
        }
    }

    /// The install offer prompts only where a human can answer: a TTY without
    /// `--noconfirm`. Everywhere else it refuses — never a surprise clone.
    #[test]
    fn install_offer_prompts_only_on_an_interactive_tty() {
        assert_eq!(
            consent_mode(RefreshReason::InstallOffer, false, true),
            ConsentMode::Prompt
        );
        assert_eq!(
            consent_mode(RefreshReason::InstallOffer, false, false),
            ConsentMode::Refuse
        );
        assert_eq!(
            consent_mode(RefreshReason::InstallOffer, true, true),
            ConsentMode::Refuse
        );
    }

    /// An explicit CLI sync carries the intent even on a pipe — the prompt's
    /// read-line fallback (EOF ⇒ yes, piped `n` ⇒ decline) still applies, so
    /// scripts keep both levers.
    #[test]
    fn explicit_cli_syncs_prompt_on_and_off_tty() {
        for reason in [RefreshReason::ExplicitSync, RefreshReason::ForceReclone] {
            for tty in [true, false] {
                assert_eq!(
                    consent_mode(reason, false, tty),
                    ConsentMode::Prompt,
                    "{reason:?} tty={tty}"
                );
            }
        }
    }

    /// The shell asked its own three-way question at launch: `refresh aur`
    /// after that is pre-consented (no second Y/n), while the bare `refresh`
    /// and `upgrade` must never spring the clone on a user who answered
    /// "later".
    #[test]
    fn shell_aur_sync_preconsented_and_bare_refresh_and_upgrade_refuse() {
        assert_eq!(
            consent_mode(RefreshReason::ShellAurSync, false, true),
            ConsentMode::AutoYes
        );
        for reason in [RefreshReason::ShellRefresh, RefreshReason::ShellUpgrade] {
            assert_eq!(consent_mode(reason, false, true), ConsentMode::Refuse);
            assert_eq!(
                refusal_cause(reason),
                SkipCause::NotSetUp,
                "{reason:?}'s skip must read as \"not set up\", not \"non-interactive\""
            );
        }
        assert_eq!(
            refusal_cause(RefreshReason::IndexResync),
            SkipCause::NonInteractive
        );
    }

    /// The implicit schema-bump resync may ask a present human but must never
    /// bootstrap behind a pipe.
    #[test]
    fn implicit_resync_prompts_only_on_a_tty() {
        assert_eq!(
            consent_mode(RefreshReason::IndexResync, false, true),
            ConsentMode::Prompt
        );
        assert_eq!(
            consent_mode(RefreshReason::IndexResync, false, false),
            ConsentMode::Refuse
        );
    }

    /// `aur = false` beats every trigger and every mirror state.
    #[test]
    fn disabled_config_skips_everything() {
        for state in [
            MirrorState::Ready,
            MirrorState::Interrupted,
            MirrorState::Absent,
        ] {
            for reason in ALL_REASONS {
                assert_eq!(
                    decide(false, state, reason),
                    AurAction::Skip(SkipCause::Disabled),
                    "{state:?} {reason:?}"
                );
            }
        }
    }

    /// A healthy mirror fetches incrementally — no consent involved — except
    /// under `-Syy`, which deliberately re-clones.
    #[test]
    fn ready_mirror_fetches_unless_force_recloned() {
        for reason in [
            RefreshReason::ExplicitSync,
            RefreshReason::ShellAurSync,
            RefreshReason::ShellRefresh,
            RefreshReason::ShellUpgrade,
            RefreshReason::IndexResync,
            RefreshReason::InstallOffer,
        ] {
            assert_eq!(
                decide(true, MirrorState::Ready, reason),
                AurAction::Fetch,
                "{reason:?}"
            );
        }
        assert_eq!(
            decide(true, MirrorState::Ready, RefreshReason::ForceReclone),
            AurAction::Bootstrap(BootstrapKind::ForcedReclone)
        );
    }

    /// Missing or interrupted mirrors want a bootstrap whatever the trigger;
    /// the kind picks the announcement copy.
    #[test]
    fn missing_and_interrupted_mirrors_want_bootstrap() {
        for reason in ALL_REASONS {
            assert_eq!(
                decide(true, MirrorState::Absent, reason),
                AurAction::Bootstrap(BootstrapKind::FirstRun),
                "{reason:?}"
            );
            assert_eq!(
                decide(true, MirrorState::Interrupted, reason),
                AurAction::Bootstrap(BootstrapKind::InterruptedRedo),
                "{reason:?}"
            );
        }
    }

    /// The cause reads sensibly inside "AUR refresh skipped ({cause})".
    #[test]
    fn skip_cause_wording() {
        assert_eq!(SkipCause::Disabled.to_string(), "aur = false in config");
        assert_eq!(SkipCause::Declined.to_string(), "declined");
        assert_eq!(SkipCause::NotSetUp.to_string(), "AUR not set up");
        assert_eq!(SkipCause::NonInteractive.to_string(), "non-interactive run");
        assert_eq!(SkipCause::NotRequested.to_string(), "not requested");
    }
}
