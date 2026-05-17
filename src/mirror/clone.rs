//! Bootstrap full bare clone of the AUR mirror via gix.
//!
//! Uses [`gix::prepare_clone_bare`] + `fetch_only` with our indicatif-backed
//! progress reporter. No subprocess, no libgit2.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::ui;
use gix::remote::Direction;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use tracing::{info, instrument};

/// Bare-clone the configured mirror URL into `dest`, with a live progress UI.
#[instrument(skip(cfg))]
pub fn bootstrap_clone(cfg: &Config, dest: &Path) -> Result<()> {
    info!(url = %cfg.mirror_url, dest = %dest.display(), "gix clone --bare");

    let mut progress = ui::GixProgress::new("clone");
    let interrupt = AtomicBool::new(false);

    // gix's default clone refspec is `+refs/heads/*:refs/remotes/<name>/*`
    // (matches `git clone`), but for a bare AUR mirror we want the
    // `git clone --bare` semantics: branches land directly under
    // `refs/heads/*` so collect_branches() / is_bootstrapped() see them.
    let mut prep = gix::prepare_clone_bare(cfg.mirror_url.as_str(), dest)
        .map_err(|e| Error::Gix(format!("prepare_clone_bare: {e}")))?
        .configure_remote(|mut remote| {
            remote.replace_refspecs(["+refs/heads/*:refs/heads/*"], Direction::Fetch)?;
            Ok(remote)
        });
    let (_repo, _outcome) = prep
        .fetch_only(&mut progress, &interrupt)
        .map_err(|e| Error::Gix(format!("fetch_only: {e}")))?;

    progress.finish();
    info!("clone complete");
    Ok(())
}
