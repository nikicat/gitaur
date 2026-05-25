//! Bare clone of the AUR mirror plus per-pkgbase build-directory materialization.
//!
//! Built on [`gix`] (gitoxide), pure Rust. No subprocess, no libgit2.
//! Per-pkgbase directories are *materialized* from the bare repo's tree
//! objects rather than created via `git worktree add` — gitaur owns those
//! directories, so a plain checkout is sufficient.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::index;
use crate::paths;
use crate::ui;
use gix::protocol::transport::client::blocking_io::http;
use std::any::Any;
use std::path::{Path, PathBuf};

pub mod clone;
pub mod fetch;
pub mod sideband;
pub mod worktree;

/// Build the `http::Options` payload gix's curl transport downcasts in its
/// `configure()` hook. Sets `lowSpeedLimit=1`, `lowSpeedTime=cfg.idle_secs`
/// so the connection aborts after `idle_secs` of <1 byte/s — i.e., true
/// silence from the remote, not a total deadline.
pub(crate) fn http_transport_options(cfg: &Config) -> http::Options {
    let mut opts = http::Options::default();
    if cfg.mirror_idle_timeout_secs > 0 {
        opts.low_speed_limit_bytes_per_second = 1;
        opts.low_speed_time_seconds = cfg.mirror_idle_timeout_secs;
    }
    opts
}

/// `set_transport_options` wants `Box<dyn Any>`; wrap once at the call site.
pub(crate) fn boxed_http_options(cfg: &Config) -> Box<dyn Any + Send + Sync> {
    Box::new(http_transport_options(cfg))
}

/// Handle to the bare AUR mirror on disk.
pub struct MirrorRepo {
    /// On-disk path of the bare repo.
    pub path: PathBuf,
    /// Open gix repo. `gix::Repository` is `Send`+`Sync` so workers may share it.
    pub repo: gix::Repository,
}

impl MirrorRepo {
    /// Open the existing bare clone at `path` without any network access.
    pub fn open(path: &Path) -> Result<Self> {
        let repo =
            gix::open(path).map_err(|e| Error::Gix(format!("open {}: {e}", path.display())))?;
        Ok(Self {
            path: path.to_path_buf(),
            repo,
        })
    }
}

/// Fetch mirror updates and incrementally refresh the on-disk index.
///
/// `force_reclone` (set by `gitaur -Syy`) blows away the existing bare clone
/// and re-bootstraps from scratch, regardless of whether the current clone
/// looks healthy. Use when the on-disk repo is suspected to be corrupted or
/// when you want a clean baseline.
pub fn cmd_refresh(cfg: &Config, force_reclone: bool) -> Result<()> {
    let path = paths::aur_repo_path();

    if force_reclone && path.exists() {
        ui::warn("re-clone forced (-Syy); removing existing mirror");
        std::fs::remove_dir_all(&path)?;
    }

    if !is_bootstrapped(&path) {
        if path.exists() {
            ui::warn("previous bootstrap was interrupted; redoing clone");
            std::fs::remove_dir_all(&path)?;
        } else if !force_reclone {
            ui::info("first run: cloning AUR mirror (this takes a few minutes)");
        }
        clone::bootstrap_clone(cfg, &path)?;
        ui::info("building index");
        let mirror = MirrorRepo::open(&path)?;
        let idx = index::build::full_build(cfg, &mirror)?;
        index::save(&idx, &paths::index_path())?;
        ui::info("index built");
        return Ok(());
    }

    ui::info("refreshing AUR mirror");
    let mirror = MirrorRepo::open(&path)?;
    let updates = fetch::incremental_fetch(cfg, &mirror)?;

    // Load the existing index if any. A failed load (rkyv validation, schema
    // mismatch after a gitaur upgrade, etc.) is **recovered from in-place**
    // by falling back to a full rebuild — otherwise the user would be stuck
    // in a loop where `-Sy` errors out before it can rebuild.
    let idx_path = paths::index_path();
    let existing = if idx_path.exists() {
        match index::load(&idx_path) {
            Ok(idx) => Some(idx),
            Err(e) => {
                ui::warn(&format!(
                    "existing index unreadable, rebuilding from scratch: {e}"
                ));
                None
            }
        }
    } else {
        None
    };

    match existing {
        Some(mut idx) if !updates.is_empty() => {
            index::update::incremental_update(&mirror, &updates, &mut idx)?;
            index::save(&idx, &idx_path)?;
            ui::note(&format!("{} ref(s) updated", updates.len()));
        }
        Some(_) => {
            ui::note("no ref updates");
        }
        None => {
            ui::info("building index");
            let idx = index::build::full_build(cfg, &mirror)?;
            index::save(&idx, &idx_path)?;
            ui::info("index built");
        }
    }
    Ok(())
}

/// A bare clone counts as "bootstrapped" if it has at least one branch under
/// `refs/heads/*`. gix writes refs after the pack is durable, so absence of
/// refs ⇒ the previous clone never finished.
fn is_bootstrapped(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    let Ok(repo) = gix::open(path) else {
        return false;
    };
    let Ok(refs) = repo.references() else {
        return false;
    };
    let Ok(mut iter) = refs.prefixed("refs/heads/") else {
        return false;
    };
    iter.next().is_some()
}
