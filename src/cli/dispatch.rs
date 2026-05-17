//! Decide whether to handle an operation natively or pass through to pacman.

use crate::build;
use crate::cli::flags::{self, PacFlags};
use crate::cli::Cli;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::index;
use crate::mirror;
use crate::pacman::invoke;

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
        Some(other) => Err(Error::other(format!(
            "unsupported gitaur op `-{other}` (pacman pass-through goes via the pre-scan, this dispatch is `-S` only)"
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
        let mut pac_args = vec!["-Syu".to_string()];
        if noconfirm {
            pac_args.push("--noconfirm".into());
        }
        invoke::exec_pacman(cfg, &pac_args)?;
        build::cmd_sysupgrade(cfg, cfg.devel || devel, noconfirm)?;
    }

    if !f.positional.is_empty() {
        build::cmd_install(cfg, &f.positional, noconfirm, asdeps)?;
    } else if !upgrade && !refresh {
        return Err(Error::other("no targets specified"));
    }

    Ok(0)
}
