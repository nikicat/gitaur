//! Top-level CLI entry. Pre-scans argv for the pacman operation letter; if
//! it's an operation gitaur doesn't own (`Q`/`R`/`T`/`D`/`F`/`U`), we skip
//! clap entirely and forward raw to `pacman` so unknown pacman short flags
//! aren't rejected. Otherwise clap parses our gitaur-owned flags + supplies
//! auto-generated `--help`/`--version`.

use crate::config::Config;
use crate::error::Result;
use crate::pacman::invoke;
use crate::paths;
use crate::ui;
use clap::Parser;

pub mod dispatch;
pub mod flags;

/// yay-like AUR helper backed by the github.com/archlinux/aur mirror.
///
/// Pacman operations gitaur doesn't own (`-Q`, `-R`, `-T`, `-D`, `-F`, `-U`)
/// are forwarded to `pacman` unchanged — run them with `pacman -Sh` / `-Qh`
/// for their own option lists. Run gitaur with no args to refresh the AUR
/// mirror + index (equivalent to `-Sy`).
#[derive(Parser, Debug)]
#[command(
    name = "gitaur",
    version,
    about,
    long_about = None,
    after_help = AFTER_HELP,
    disable_help_subcommand = true,
)]
pub struct Cli {
    /// Include VCS pkgs (-git/-svn/-hg/-bzr) when running -Syu.
    #[arg(long, global = true)]
    pub devel: bool,

    /// Skip prompts; auto-accept installs.
    #[arg(long, global = true)]
    pub noconfirm: bool,

    /// Mark installed packages as dependencies (forwarded to `pacman -U --asdeps`).
    #[arg(long, global = true)]
    pub asdeps: bool,

    /// Show the resolved execution plan and exit without making changes.
    #[arg(long, global = true)]
    pub plan: bool,

    /// Color mode: `auto` (default), `always`, `never`.
    #[arg(long, global = true)]
    pub color: Option<String>,

    /// Pacman-style `-S` operation + targets (clustered short flags supported:
    /// `-S`, `-Sy`, `-Syu`, `-Ss`, `-Si`, `-Sc`, `-Scc`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

const AFTER_HELP: &str = "GITAUR-OWNED OPERATIONS:\n\
  -S <pkg>...    install AUR packages (recursive deps, batched sudo)\n\
  -Sy            refresh AUR mirror + rebuild index (incremental fetch)\n\
  -Syy           force full re-clone of the AUR mirror (~8–9 min)\n\
  -Syu           pacman -Syu, then AUR upgrades\n\
  -Ss <regex>    search AUR by name/desc/provides\n\
  -Si <pkg>      show AUR package info\n\
  -Sc / -Scc     clean built worktrees (cc also drops state.db)\n\
\n\
PLAN PREVIEW:\n\
  --plan         print the resolved execution plan and exit without making changes\n\
\n\
PASS-THROUGH (raw `pacman` — clap doesn't parse these):\n\
  -Q, -R, -T, -D, -F, -U, and any flags they accept\n\
\n\
ENVIRONMENT:\n\
  RUST_LOG=gitaur=debug    raise console tracing level\n\
  EDITOR                   used by PKGBUILD review's `edit` choice\n\
\n\
Execution logs (debug level, last 10 runs): $XDG_STATE_HOME/gitaur/logs/\n\
Persistent settings: ~/.config/gitaur/config.toml";

/// Top-level entry. Returns the desired process exit code.
pub fn run() -> Result<u8> {
    let cfg = Config::load()?;
    paths::ensure_state_dir()?;

    let raw_argv: Vec<String> = std::env::args().skip(1).collect();

    // Pre-scan: if the first operation letter is pacman-owned, forward
    // verbatim and never let clap see it (clap would reject unknown short
    // flags like `-Rns`).
    if let Some(op) = first_op_letter(&raw_argv) {
        if matches!(op, 'Q' | 'R' | 'T' | 'D' | 'F' | 'U') {
            // No color/log setup needed for pure pass-through.
            return invoke::exec_pacman(&cfg, &raw_argv);
        }
    }

    let cli = Cli::parse();
    let mode = cli
        .color
        .as_deref()
        .map_or_else(|| cfg.color_mode(), parse_color_mode);
    ui::set_color(mode);
    dispatch::dispatch(&cfg, &cli)
}

/// Scan argv for the first short-flag uppercase letter (`-S` / `-Q` / …).
/// Long flags (`--help`) and positional args are ignored.
fn first_op_letter(argv: &[String]) -> Option<char> {
    argv.iter().find_map(|a| {
        let rest = a.strip_prefix('-')?;
        if rest.starts_with('-') {
            return None; // long flag
        }
        rest.chars().next().filter(char::is_ascii_uppercase)
    })
}

fn parse_color_mode(s: &str) -> ui::ColorMode {
    match s {
        "always" => ui::ColorMode::Always,
        "never" => ui::ColorMode::Never,
        _ => ui::ColorMode::Auto,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn detects_pacman_ops() {
        assert_eq!(first_op_letter(&argv(&["-Rns", "vim"])), Some('R'));
        assert_eq!(first_op_letter(&argv(&["-Qe"])), Some('Q'));
        assert_eq!(first_op_letter(&argv(&["-U", "x.pkg.tar.zst"])), Some('U'));
    }

    #[test]
    fn detects_gitaur_ops() {
        assert_eq!(first_op_letter(&argv(&["-Syu"])), Some('S'));
        assert_eq!(first_op_letter(&argv(&["-Ss", "vim"])), Some('S'));
    }

    #[test]
    fn skips_long_flags_and_positionals() {
        assert_eq!(first_op_letter(&argv(&["--help"])), None);
        assert_eq!(first_op_letter(&argv(&["--noconfirm", "-Sy"])), Some('S'));
        assert_eq!(first_op_letter(&argv(&["vim"])), None);
        assert_eq!(first_op_letter(&argv(&[])), None);
    }

    #[test]
    fn long_flag_before_pacman_op_still_routes_to_pacman() {
        // `gitaur --noconfirm -Rns vim` must detect the `-R` even when a
        // long flag precedes it, so the full argv (including `--noconfirm`,
        // which pacman also accepts) goes through to pacman unmodified.
        assert_eq!(
            first_op_letter(&argv(&["--noconfirm", "-Rns", "vim"])),
            Some('R')
        );
    }

    #[test]
    fn lowercase_short_flags_ignored() {
        // `-y` alone (without operation) would be malformed; our scanner only
        // claims an op when the letter is uppercase.
        assert_eq!(first_op_letter(&argv(&["-y", "vim"])), None);
    }
}
