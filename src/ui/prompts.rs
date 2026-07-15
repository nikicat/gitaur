//! Interactive prompts (`y/n` + per-pkgname pickers).

use super::note;
use crate::names::{PkgBase, PkgName};

use dialoguer::{Confirm, MultiSelect};
use std::io::{BufRead, IsTerminal, Write};

/// Y/n confirmation prompt with `Y` default. Honors `noconfirm` to auto-accept.
///
/// Falls back to a plain `stdin.read_line` when stdin is not a TTY so callers
/// can pipe an answer (`echo n | aurox -S foo`), matching pacman/yay UX.
pub fn confirm(prompt: &str, noconfirm: bool) -> std::io::Result<bool> {
    if noconfirm {
        return Ok(true);
    }
    interact(prompt, true)
}

/// y/N confirmation prompt with `N` default — for "are you sure you want to
/// override the safety check?" gates, where walking away must mean *no*.
///
/// Deliberately no `noconfirm` parameter: an auto-answer would either bypass
/// the safety (`true`) or dead-end a scripted run (`false`), so the caller
/// decides what a non-interactive run does *before* prompting.
pub fn confirm_default_no(prompt: &str) -> std::io::Result<bool> {
    interact(prompt, false)
}

/// Shared prompt body: dialoguer on a TTY, a plain `read_line` fallback
/// otherwise (so tests and pipes can feed an answer). An empty line or EOF
/// takes `default`; only an explicit y/n overrides it.
fn interact(prompt: &str, default: bool) -> std::io::Result<bool> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let hint = if default { "[Y/n]" } else { "[y/N]" };
        let mut out = std::io::stdout().lock();
        write!(out, "{prompt} {hint} ")?;
        out.flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(default);
        }
        return Ok(parse_answer(&line, default));
    }
    Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact()
        .map_err(std::io::Error::other)
}

/// Map one piped answer line to a decision: an explicit y/n wins; an empty
/// line or anything unrecognized takes `default`.
fn parse_answer(line: &str, default: bool) -> bool {
    match line.trim() {
        "y" | "Y" | "yes" | "Yes" | "YES" => true,
        "n" | "N" | "no" | "No" | "NO" => false,
        _ => default,
    }
}

/// The shell's first-launch answer to "sync the AUR now?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurSetupChoice {
    /// Run the one-time bootstrap clone right now.
    SyncNow,
    /// Pacman-only from now on — persisted as `aur = false` in config.toml.
    PacmanOnly,
    /// Pacman-only for this session; ask again next launch. The safe
    /// default: walking away (or mashing Enter) must not start a ~2 GiB
    /// download or write config.
    Later,
}

/// The shell's first-launch three-way question, asked when the AUR is
/// enabled but was never synced.
///
/// A plain line prompt (not dialoguer) so the same EOF/pipe semantics as
/// [`confirm`] apply and PTY tests can drive it.
pub fn aur_setup_prompt() -> std::io::Result<AurSetupChoice> {
    super::info("the AUR isn't set up yet — aurox mirrors the whole AUR as one git repo");
    note(
        "one-time ~2 GiB download, ~2.5 GiB on disk, ~10 min; afterwards refreshes are small incremental fetches",
    );
    let mut out = std::io::stdout().lock();
    write!(
        out,
        "sync the AUR now? [y]es / [n]o, pacman-only from now on / [s]kip this session only: "
    )?;
    out.flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(parse_setup_answer(&line))
}

/// Map one answer line to a [`AurSetupChoice`]: explicit y/n wins; an empty
/// line, EOF, or anything unrecognized takes the safe `Later` default.
fn parse_setup_answer(line: &str) -> AurSetupChoice {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => AurSetupChoice::SyncNow,
        "n" | "no" => AurSetupChoice::PacmanOnly,
        _ => AurSetupChoice::Later,
    }
}

/// Ask the user which pkgnames of a split pkgbase to install.
///
/// makepkg packages every pkgname of a split PKGBUILD in one go (there's no
/// flag to skip), but `aurox` filters the resulting `.pkg.tar.zst` set
/// before `pacman -U` runs — so **unselected pkgnames are built but never
/// installed**. Selected pkgnames are installed as `Explicit`.
///
/// Short-circuits without prompting when:
///   * the pkgbase has a single pkgname (no real choice — just inform);
///   * `noconfirm` is set (auto-select every pkgname).
pub fn select_pkgnames(
    pkgbase: &PkgBase,
    pkgnames: &[PkgName],
    noconfirm: bool,
) -> std::io::Result<Vec<PkgName>> {
    if pkgnames.len() <= 1 {
        if let Some(only) = pkgnames.first()
            && !pkgbase.matches_pkgname(only)
        {
            note(&format!("resolved pkgbase `{pkgbase}` → `{only}`"));
        }
        return Ok(pkgnames.to_vec());
    }
    if noconfirm {
        return Ok(pkgnames.to_vec());
    }
    // `dialoguer::MultiSelect::items` takes anything that implements
    // `ToString`. `PkgName`'s `Display` impl satisfies it without us
    // materialising a `Vec<String>` mid-call.
    let chosen = MultiSelect::new()
        .with_prompt(format!(
            "[{pkgbase}] split package — pick pkgnames to install \
             (unselected are built but skipped at install time)"
        ))
        .items(pkgnames)
        .defaults(&vec![true; pkgnames.len()])
        .interact()
        .map_err(std::io::Error::other)?;
    Ok(chosen.into_iter().map(|i| pkgnames[i].clone()).collect())
}

#[cfg(test)]
mod tests {
    use super::{AurSetupChoice, parse_answer, parse_setup_answer};

    #[test]
    fn explicit_answers_override_either_default() {
        for yes in ["y", "Y", "yes", "Yes", "YES", " yes\n"] {
            assert!(parse_answer(yes, false), "{yes:?} must read as yes");
        }
        for no in ["n", "N", "no", "No", "NO", " no\n"] {
            assert!(!parse_answer(no, true), "{no:?} must read as no");
        }
    }

    #[test]
    fn empty_or_noise_takes_the_default() {
        for line in ["", "\n", "maybe", "j", "yep"] {
            assert!(parse_answer(line, true), "{line:?} with default=yes");
            assert!(!parse_answer(line, false), "{line:?} with default=no");
        }
    }

    /// The launch question: y syncs, n goes pacman-only, and everything else
    /// — Enter, EOF, typos, "s" — takes the safe skip-this-session default
    /// (never a surprise download, never a config write).
    #[test]
    fn setup_answers_parse_with_later_default() {
        for line in ["y", "Y", "yes", " yes\n"] {
            assert_eq!(
                parse_setup_answer(line),
                AurSetupChoice::SyncNow,
                "{line:?}"
            );
        }
        for line in ["n", "no", "NO"] {
            assert_eq!(
                parse_setup_answer(line),
                AurSetupChoice::PacmanOnly,
                "{line:?}"
            );
        }
        for line in ["", "\n", "s", "skip", "wat"] {
            assert_eq!(parse_setup_answer(line), AurSetupChoice::Later, "{line:?}");
        }
    }
}
