//! Decide whether to handle an operation natively or pass through to pacman.

use crate::build;
use crate::cli::Cli;
use crate::cli::flags::{self, PacFlags};
use crate::cli::search;
use crate::cli::shell;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::index;
use crate::mirror;
use crate::names::{PkgTarget, SearchTerm};
use crate::pacman::invoke;
use crate::ui;
use std::io::IsTerminal;

/// Top-level routing entry — clap already pre-scanned for pacman-owned ops,
/// so by this point `cli.args` is gitaur's responsibility (`-S` family,
/// the bare-arg yay shortcuts, or none-at-all).
pub fn dispatch(cfg: &Config, cli: &Cli) -> Result<u8> {
    let argv = &cli.args;
    let f = flags::parse(argv);

    // yay parity: no operation letter and no positional targets. Interactively
    // this opens the shell (REPL) — see `cli::shell`; in phase 1 its `upgrade`
    // command bridges to the iterative upgrade loop. A non-interactive run
    // (--noconfirm, piped stdin, cron) does a single `-Syu` pass like explicit
    // `-Syu`. Replaces an older "no-args = -Sy only" shortcut: bare `yay` /
    // bare `paru` both upgrade, and gitaur's lone outlier was a surprise rather
    // than a feature.
    if f.op.is_none() && f.positional.is_empty() {
        let interactive = !cli.noconfirm && std::io::stdin().is_terminal();
        if interactive {
            return shell::run(cfg, cli.devel || cfg.devel);
        }
        // Non-interactive bare `gaur` (cron / pipe / `--noconfirm`) is a plain
        // `pacman -Syu --noconfirm` — there's no human to answer pacman's
        // prompts, so `--noconfirm` is required (the explicit `-Su` flag keeps
        // the user's own flags instead).
        return invoke::exec_pacman(cfg, &["-Syu".to_owned(), "--noconfirm".to_owned()]);
    }

    match f.op {
        Some('S') => handle_s(cfg, cli, &f, argv),
        // Pre-scan in `cli::run` only routes the bare `-Qu` form here; every
        // other Q variant is plain pacman territory and never reaches dispatch.
        Some('Q') => build::cmd_query_upgrades(cfg, cli.devel || cfg.devel || f.has_long("devel")),
        Some(other) => Err(Error::other(format!(
            "unsupported gitaur op `-{other}` (pacman pass-through goes via the pre-scan, this dispatch is `-S` / `-Qu` only)"
        ))),
        // yay parity: `gaur <term>...` with no operation letter is a
        // fuzzy search across the AUR index → interactive multi-select →
        // install. The empty-positional branch above already absorbed the
        // no-op-and-no-target case, so reaching here means we have terms.
        None => search::cmd_search_install(cfg, cli, &search_terms(&f.positional)),
    }
}

/// Promote raw positional argv into typed [`SearchTerm`]s — the boundary where
/// unclassified CLI strings become search patterns for the `-Ss` / bare-search
/// paths.
fn search_terms(positional: &[String]) -> Vec<SearchTerm> {
    positional.iter().cloned().map(SearchTerm::from).collect()
}

/// Promote raw positional argv into unclassified [`PkgTarget`]s — the boundary
/// where CLI strings become package references for the `-Si` path.
fn pkg_targets(positional: &[String]) -> Vec<PkgTarget> {
    positional.iter().cloned().map(PkgTarget::from).collect()
}

/// Handle the `-S` family (`-S`, `-Sy`, `-Syu`, `-Ss`, `-Si`, `-Sc`).
fn handle_s(cfg: &Config, cli: &Cli, f: &PacFlags, argv: &[String]) -> Result<u8> {
    // `--noconfirm` / `--asdeps` / `--devel` may appear before *or* after the
    // operation (`gaur --noconfirm -S foo` vs `gaur -S --noconfirm foo`).
    // clap's `trailing_var_arg` captures everything after `-S`, so flags that
    // followed the op are inside `argv` and never reach `cli.*`. Merge here.
    let noconfirm = cli.noconfirm || f.has_long("noconfirm");
    let asdeps = cli.asdeps || f.has_long("asdeps");

    if f.has('h') || f.has_long("help") {
        // Same auto-generated help as `gaur --help` — clap already lists
        // every gitaur-owned flag (with its doc comment) plus the operations
        // section from `after_help`. No reason to maintain a separate copy.
        use clap::CommandFactory;
        Cli::command().print_help().ok();
        println!();
        return Ok(0);
    }

    if f.has('s') {
        return index::cmd_search(cfg, &search_terms(&f.positional));
    }
    if f.has('i') {
        return index::cmd_info(cfg, &pkg_targets(&f.positional));
    }
    if f.has('c') {
        return build::cmd_clean(cfg, argv);
    }

    // `-Su` (system upgrade) is pacman's job, not gitaur's: the interactive
    // shell (`gaur` → `upgrade`) owns the AUR-aware upgrade flow now, so the
    // explicit flag just passes the whole argv through to `pacman -Su…` (same
    // as `-Q`/`-R`/etc.). gitaur's own `-Sy` mirror refresh is deliberately not
    // run here — `pacman -Sy` syncs its own DBs; refresh the AUR mirror with a
    // standalone `gaur -Sy` or via the shell.
    if f.has('u') {
        return invoke::exec_pacman(cfg, argv);
    }

    let refresh = f.has('y');
    // Pacman convention: -Sy is incremental, -Syy forces a full re-fetch.
    // For gitaur that means re-cloning the bare mirror from scratch.
    let force_reclone = f.op_letters.iter().filter(|c| **c == 'y').count() >= 2;

    if refresh {
        mirror::cmd_refresh(cfg, force_reclone)?;
    }

    if !f.positional.is_empty() {
        // cmd_install returns 1 when the AUR pipeline finished with at
        // least one build failure or dep-block — the summary already
        // explains what happened, so we just propagate the exit code so
        // shells / `||` chains see the failure.
        // Direct `-S` argv has no per-target hint — expand will derive one
        // from the spec when it rewrites (pkgname / provides paths).
        let targets: Vec<build::Target> = f
            .positional
            .iter()
            .cloned()
            .map(build::Target::bare)
            .collect();
        return build::cmd_install(cfg, &targets, noconfirm, asdeps, false);
    } else if !refresh {
        return Err(Error::other("no targets specified"));
    }

    Ok(0)
}

/// Drive `pacman -Syu` for the selected repo packages.
///
/// If the user deselected any rows, those pkgnames become `--ignore=<csv>` —
/// pacman still resolves the full upgrade graph (partial-upgrade safety) but
/// pins the listed versions. If every repo upgrade was deselected we skip the
/// pacman call entirely; there's nothing to do (and no point asking for sudo).
///
/// Driven by the shell's `apply` (the repo half of an upgrade transaction);
/// the explicit `-Syu` flag bypasses this entirely as a `pacman` passthrough.
///
/// `Ok(())` is "the repo upgrade ran (or there was nothing to do)"; a non-zero
/// pacman exit surfaces as [`Error::PacmanExit`]. (There's no meaningful success
/// code to return — `exec_pacman` yields `Ok(0)` or an error, never `Ok(n)`.)
pub(crate) fn run_repo_upgrade(cfg: &Config, sel: &ui::UpgradeSelection) -> Result<()> {
    if sel.repo.is_empty() {
        return Ok(());
    }
    if !sel.repo_skipped.is_empty() {
        ui::warn(&format!(
            "partial upgrade — pinning {} repo package(s) via --ignore (Arch officially discourages partial upgrades)",
            sel.repo_skipped.len()
        ));
    }
    let mut argv: Vec<String> = vec!["-Syu".into(), "--noconfirm".into()];
    if !sel.repo_skipped.is_empty() {
        argv.push("--ignore".into());
        argv.push(sel.repo_skipped.join(","));
    }
    invoke::exec_pacman(cfg, &argv).map(|_| ())
}
