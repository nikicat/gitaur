//! Top-level CLI entry.
//!
//! Pre-scans argv for the pacman operation letter; if it's an operation
//! aurox doesn't own (`Q`/`R`/`T`/`D`/`F`/`U`), we skip clap entirely and
//! forward raw to `pacman` so unknown pacman short flags aren't rejected.
//! Otherwise clap parses our aurox-owned flags + supplies auto-generated
//! `--help`/`--version`.

use crate::config::ConfigHandle;
use crate::error::Result;
use crate::pacman::invoke;
use crate::paths;
use crate::runopts::{self, RunOpts};
use crate::ui;
use clap::Parser;

pub mod dispatch;
pub mod flags;
pub mod search;
pub mod shell;

/// yay-like AUR helper backed by the github.com/archlinux/aur mirror.
///
/// Pacman operations aurox doesn't own (`-Q`, `-R`, `-T`, `-D`, `-F`, `-U`)
/// are forwarded to `pacman` unchanged — run them with `pacman -Sh` / `-Qh`
/// for their own option lists. Two yay parity shortcuts:
///   * `aurox`            → `-Syu` (refresh + upgrade with picker)
///   * `aurox <term>...`  → AUR fuzzy search + interactive install picker
#[derive(Parser, Debug)]
#[command(
    name = "aurox",
    version,
    about,
    long_about = None,
    after_help = AFTER_HELP,
    disable_help_subcommand = true,
)]
// Each bool is an independent on/off CLI flag, not packed state — the
// "fold into an enum" remedy the lint suggests doesn't apply to clap args.
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    /// Include VCS pkgs (-git/-svn/-hg/-bzr) when running -Syu.
    #[arg(long, global = true)]
    pub devel: bool,

    /// Skip prompts; auto-accept installs.
    #[arg(long, global = true)]
    pub noconfirm: bool,

    /// Don't auto-rebuild the AUR index when it's from an older aurox; error
    /// out instead (rerun `aurox -Sy` yourself to rebuild).
    #[arg(long, global = true)]
    pub noresync: bool,

    /// Mark installed packages as dependencies (forwarded to `pacman -U --asdeps`).
    #[arg(long, global = true)]
    pub asdeps: bool,

    /// Color mode: `auto` (default), `always`, `never`.
    #[arg(long, global = true)]
    pub color: Option<String>,

    /// Pacman-style `-S` operation + targets (clustered short flags supported:
    /// `-S`, `-Sy`, `-Syu`, `-Ss`, `-Si`, `-Sc`, `-Scc`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

const AFTER_HELP: &str = "AUROX-OWNED OPERATIONS:\n\
  -S <pkg>...    install AUR packages (plan shown, one confirm, batched sudo)\n\
  -Sy            refresh AUR mirror + rebuild index (first run asks: ~2 GiB once)\n\
  -Syy           force full re-clone of the AUR mirror (~10 min; asks first)\n\
  -Syu           pacman -Syu + AUR upgrades (plan shown, one confirm up front)\n\
  -Ss <regex>    search repos + AUR by name/desc/provides\n\
  -Si <pkg>      show package info (repos + AUR; repo wins a shared name)\n\
  -Sc / -Scc     remove built worktrees + pass -Sc/-Scc through to pacman\n\
  -Qu            list upgrades from repos + AUR, no sudo (dry-run for -Syu)\n\
\n\
YAY PARITY SHORTCUTS:\n\
  aurox                 run -Syu (refresh + upgrade)\n\
  aurox <term>...       fuzzy repos+AUR search → shell seeded with the matches\n\
\n\
PASS-THROUGH (raw `pacman` — clap doesn't parse these):\n\
  -Q (except -Qu), -R, -T, -D, -F, -U, and any flags they accept\n\
\n\
ENVIRONMENT:\n\
  RUST_LOG=aurox=debug    raise console tracing level\n\
  EDITOR                   used by PKGBUILD review's `edit` choice\n\
\n\
Execution logs (debug level, last 10 runs): $XDG_STATE_HOME/aurox/logs/\n\
Persistent settings: ~/.config/aurox/config.toml";

/// Top-level entry. Returns the desired process exit code.
pub fn run() -> Result<u8> {
    let config = ConfigHandle::load()?;
    let cfg = config.cfg();
    paths::ensure_state_dir()?;

    let raw_argv: Vec<String> = std::env::args().skip(1).collect();

    // Install per-run options before any code path that can reach
    // `pacman::invoke::exec_pacman` — that's the pre-scan pass-through, the
    // clap-driven dispatch, and `mirror::cmd_refresh` (which doesn't sudo
    // but is cheap to cover). `argv_has_noconfirm` on raw argv is a
    // superset of `cli.noconfirm || f.has_long("noconfirm")` since both of
    // those ultimately mean "the token appears in raw argv", so dispatch
    // doesn't need to re-install.
    runopts::set(RunOpts {
        noconfirm: runopts::argv_has_noconfirm(&raw_argv),
        noresync: runopts::argv_has_noresync(&raw_argv),
    });

    // Pre-scan: if the first operation letter is pacman-owned, forward
    // verbatim and never let clap see it (clap would reject unknown short
    // flags like `-Rns`). `-Qu` is the one Q-family op aurox owns — it
    // augments `pacman -Qu` with AUR upgrade candidates, so we let it fall
    // through to clap + dispatch.
    if let Some(op) = first_op_letter(&raw_argv) {
        if matches!(op, 'R' | 'T' | 'D' | 'F' | 'U') {
            return invoke::exec_pacman(cfg, &raw_argv);
        }
        if op == 'Q' && !is_plain_qu(&raw_argv) {
            return invoke::exec_pacman(cfg, &raw_argv);
        }
    }

    let cli = Cli::parse();
    let mode = cli
        .color
        .as_deref()
        .map_or_else(|| cfg.color_mode(), parse_color_mode);
    ui::set_color(mode);
    dispatch::dispatch(&config, &cli)
}

/// Decide whether argv is the aurox-owned `-Qu` form (merge of repo + AUR
/// upgrades) or a pure-pacman query that should pass straight through.
///
/// `run()` forwards every `-Q*` to pacman by default because clap doesn't
/// know pacman's short flags and would reject `-Qul`, `-Qe`, `-Qm`, etc.
/// Aurox only reimplements one member of the Q family — bare `-Qu` — so
/// the carve-out is intentionally narrow: cluster of nothing but `u`'s, no
/// positional filter. `-Qul`, `-Qu pkgname`, `-Quq`, … all keep pacman's
/// exact semantics by failing this check and falling through to the
/// pass-through.
fn is_plain_qu(argv: &[String]) -> bool {
    let f = flags::parse(argv);
    f.op == Some('Q')
        && !f.op_letters.is_empty()
        && f.op_letters.iter().all(|c| *c == 'u')
        && f.positional.is_empty()
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
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn detects_pacman_ops() {
        assert_eq!(first_op_letter(&argv(&["-Rns", "vim"])), Some('R'));
        assert_eq!(first_op_letter(&argv(&["-Qe"])), Some('Q'));
        assert_eq!(first_op_letter(&argv(&["-U", "x.pkg.tar.zst"])), Some('U'));
    }

    #[test]
    fn detects_aurox_ops() {
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
        // `aurox --noconfirm -Rns vim` must detect the `-R` even when a
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
