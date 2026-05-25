//! Interactive prompts (`y/n` + per-pkgname pickers).

use super::note;
use crate::names::{PkgBase, PkgName};

use dialoguer::{Confirm, MultiSelect};
use std::io::{BufRead, IsTerminal, Write};

/// Y/n confirmation prompt with `Y` default. Honors `noconfirm` to auto-accept.
///
/// Falls back to a plain `stdin.read_line` when stdin is not a TTY so callers
/// can pipe an answer (`echo n | gitaur -S foo`), matching pacman/yay UX.
pub fn confirm(prompt: &str, noconfirm: bool) -> std::io::Result<bool> {
    if noconfirm {
        return Ok(true);
    }
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut out = std::io::stdout().lock();
        write!(out, "{prompt} [Y/n] ")?;
        out.flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(true);
        }
        let answer = line.trim();
        return Ok(!matches!(answer, "n" | "N" | "no" | "No" | "NO"));
    }
    Confirm::new()
        .with_prompt(prompt)
        .default(true)
        .interact()
        .map_err(std::io::Error::other)
}

/// Ask the user which pkgnames of a split pkgbase to install.
///
/// makepkg packages every pkgname of a split PKGBUILD in one go (there's no
/// flag to skip), but `gitaur` filters the resulting `.pkg.tar.zst` set
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
