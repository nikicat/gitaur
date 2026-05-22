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
        return build::cmd_clean(cfg, argv);
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
        // Build the unified repo + AUR plan unprivileged, hand it to the
        // interactive picker, then act on the user's selection. The picker
        // falls back to the default mask under `noconfirm` or non-TTY stdin,
        // so cron / pipes keep working without prompting.
        let plan = build::collect_upgrade_plan(cfg.devel || devel)?;
        if plan.is_empty() {
            ui::info("nothing to do");
        } else {
            let sel = ui::select_upgrades(&plan, cfg, noconfirm)
                .map_err(|e| Error::other(format!("upgrade selection: {e}")))?;
            if sel.is_empty() {
                return Err(Error::UserAbort);
            }
            run_repo_upgrade(cfg, &sel)?;
            if !sel.aur.is_empty() {
                // PkgUpgrade.name is the typed foreign pkgname the picker
                // matched against the AUR index — that *is* the counterpart
                // hint we want `prepare_one` to use when classifying which
                // installed pkg this build will displace. Wrap each row as
                // a `Target` with an explicit hint so the intent travels
                // through expand → resolve → prepare instead of being
                // re-inferred from the spec string.
                let targets: Vec<build::Target> = sel
                    .aur
                    .iter()
                    .map(|p| build::Target::with_hint(p.name.clone().into_inner(), p.name.clone()))
                    .collect();
                let code = build::cmd_install(cfg, &targets, noconfirm, false, true)?;
                if code != 0 {
                    return Ok(code);
                }
            }
        }
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
    } else if !upgrade && !refresh {
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
fn run_repo_upgrade(cfg: &Config, sel: &ui::UpgradeSelection) -> Result<u8> {
    if sel.repo.is_empty() {
        return Ok(0);
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
    invoke::exec_pacman(cfg, &argv)
}
