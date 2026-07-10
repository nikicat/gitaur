//! Bootstrap full bare clone of the AUR mirror via gix.
//!
//! Uses [`gix::prepare_clone_bare`] + `fetch_only` with our indicatif-backed
//! progress reporter. No subprocess, no libgit2.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::mirror::http_transport_options;
use crate::ui;
use gix::remote::Direction;
use indicatif::MultiProgress;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use tracing::{info, instrument};

/// Bare-clone the configured mirror URL into `dest`, with a live progress UI.
///
/// Draws into the caller-owned `mp` so the clone rows share one display with
/// the parallel official-repo db sync (see [`crate::mirror::cmd_refresh`]).
#[instrument(skip(cfg, mp))]
pub fn bootstrap_clone(cfg: &Config, dest: &Path, mp: &MultiProgress) -> Result<()> {
    info!(url = %cfg.mirror_url, dest = %dest.display(), "gix clone --bare");

    let mut progress = ui::GixProgress::with_multi("clone", mp.clone());
    let net_counter = progress.net_counter();
    let interrupt = AtomicBool::new(false);

    // gix's default clone refspec is `+refs/heads/*:refs/remotes/<name>/*`
    // (matches `git clone`), but for a bare AUR mirror we want the
    // `git clone --bare` semantics: branches land directly under
    // `refs/heads/*` so collect_branches() / is_bootstrapped() see them.
    let opts = http_transport_options(cfg.bootstrap_idle_timeout_secs, net_counter);
    let mut prep = gix::prepare_clone_bare(cfg.mirror_url.as_str(), dest)
        .map_err(|e| Error::gix("prepare_clone_bare", e))?
        .configure_remote(|mut remote| {
            remote.replace_refspecs(["+refs/heads/*:refs/heads/*"], Direction::Fetch)?;
            Ok(remote)
        })
        .configure_connection(move |connection| {
            // Wire lowSpeed* into the curl transport so the bootstrap fetch
            // bails when the server stops streaming (vs. waiting on TCP retry).
            // The bootstrap-specific window must outlast GitHub's silent
            // server-side pack preparation — see `bootstrap_idle_timeout_secs`.
            // `configure_connection` may fire more than once on retry, so
            // we clone our cached `opts` each time.
            connection.set_transport_options(Box::new(opts.clone()));
            Ok(())
        });
    let (_repo, _outcome) = prep
        .fetch_only(&mut progress, &interrupt)
        .map_err(|e| Error::gix("fetch_only", e))?;

    progress.finish();
    info!("clone complete");
    Ok(())
}
