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
        return Ok(match line.trim() {
            "y" | "Y" | "yes" | "Yes" | "YES" => true,
            "n" | "N" | "no" | "No" | "NO" => false,
            _ => default,
        });
    }
    Confirm::new()
        .with_prompt(prompt)
        .default(default)
        .interact()
        .map_err(std::io::Error::other)
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
