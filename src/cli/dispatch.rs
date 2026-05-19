//! Decide whether to handle an operation natively or pass through to pacman.

use crate::build;
use crate::cli::flags::{self, PacFlags};
use crate::cli::Cli;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::index;
use crate::mirror;
use crate::pacman::invoke;
use crate::ui;

/// Top-level routing entry — clap already pre-scanned for pacman-owned ops,
/// so by this point `cli.args` is gitaur's responsibility (`-S` family or
/// none-at-all = refresh).
pub fn dispatch(cfg: &Config, cli: &Cli) -> Result<u8> {
    let argv = &cli.args;
    let f = flags::parse(argv);

    if argv.is_empty() {
        return mirror::cmd_refresh(cfg, false).map(|()| 0);
    }

    match f.op {
        Some('S') => handle_s(cfg, cli, &f, argv),
        // Pre-scan in `cli::run` only routes the bare `-Qu` form here; every
        // other Q variant is plain pacman territory and never reaches dispatch.
        Some('Q') => build::cmd_query_upgrades(cli.devel || cfg.devel || f.has_long("devel")),
        Some(other) => Err(Error::other(format!(
            "unsupported gitaur op `-{other}` (pacman pass-through goes via the pre-scan, this dispatch is `-S` / `-Qu` only)"
        ))),
        None => invoke::exec_pacman(cfg, argv),
    }
}

/// Handle the `-S` family (`-S`, `-Sy`, `-Syu`, `-Ss`, `-Si`, `-Sc`).
fn handle_s(cfg: &Config, cli: &Cli, f: &PacFlags, argv: &[String]) -> Result<u8> {
    // `--noconfirm` / `--asdeps` / `--devel` may appear before *or* after the
    // operation (`gitaur --noconfirm -S foo` vs `gitaur -S --noconfirm foo`).
    // clap's `trailing_var_arg` captures everything after `-S`, so flags that
    // followed the op are inside `argv` and never reach `cli.*`. Merge here.
    let noconfirm = cli.noconfirm || f.has_long("noconfirm");
    let asdeps = cli.asdeps || f.has_long("asdeps");
    let devel = cli.devel || f.has_long("devel");

    if f.has('h') || f.has_long("help") {
        // Same auto-generated help as `gitaur --help` — clap already lists
        // every gitaur-owned flag (with its doc comment) plus the operations
        // section from `after_help`. No reason to maintain a separate copy.
        use clap::CommandFactory;
        let _ = Cli::command().print_help();
        println!();
        return Ok(0);
    }

    if f.has('s') {
        return index::cmd_search(cfg, &f.positional);
    }
    if f.has('i') {
        return index::cmd_info(cfg, &f.positional);
    }
    if f.has('c') {
        let deep = f.op_letters.iter().filter(|c| **c == 'c').count() >= 2;
        return build::cmd_clean(cfg, deep, argv);
    }

    let refresh = f.has('y');
    let upgrade = f.has('u');
    // Pacman convention: -Sy is incremental, -Syy forces a full re-fetch.
    // For gitaur that means re-cloning the bare mirror from scratch.
    let force_reclone = f.op_letters.iter().filter(|c| **c == 'y').count() >= 2;

    if refresh {
        mirror::cmd_refresh(cfg, force_reclone)?;
    }

    if upgrade {
        // Show the union (repo + AUR) upgrade plan first — identical to
        // `gitaur -Qu`, unprivileged — gate on a single confirmation, then
        // run pacman and the AUR pipeline without re-prompting. Use `-Qu`
        // for a dry-run preview that never reaches this confirm step.
        build::cmd_query_upgrades(cfg.devel || devel)?;
        if !ui::confirm("Proceed with upgrade?", noconfirm)? {
            return Err(Error::UserAbort);
        }
        invoke::exec_pacman(cfg, &["-Syu".into(), "--noconfirm".into()])?;
        build::cmd_sysupgrade(cfg, cfg.devel || devel, noconfirm)?;
    }

    if !f.positional.is_empty() {
        build::cmd_install(cfg, &f.positional, noconfirm, asdeps, false)?;
    } else if !upgrade && !refresh {
        return Err(Error::other("no targets specified"));
    }

    Ok(0)
}
