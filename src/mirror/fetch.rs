//! Incremental fetch of the AUR mirror via gix.
//!
//! Returns a per-ref delta vector ([`RefUpdate`]) compatible with the rest of
//! the index pipeline.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::mirror::MirrorRepo;
use crate::ui::GixProgress;
use gix::bstr::ByteSlice;
use gix::remote::{ref_map::Options as RefMapOptions, Direction};
use gix::ObjectId;
use std::sync::atomic::AtomicBool;
use tracing::{debug, info, instrument};

/// One refname change reported by the fetch.
#[derive(Debug, Clone)]
pub struct RefUpdate {
    /// Branch name (without `refs/heads/`).
    pub refname: String,
    /// Previous tip; `None` if the ref was newly created.
    pub old_oid: Option<ObjectId>,
    /// New tip; `None` if the ref was deleted.
    pub new_oid: Option<ObjectId>,
}

/// Fetch `refs/heads/*` from the mirror remote and collect [`RefUpdate`]s.
#[instrument(skip(_cfg, mirror))]
pub fn incremental_fetch(_cfg: &Config, mirror: &MirrorRepo) -> Result<Vec<RefUpdate>> {
    let mut progress = GixProgress::new("fetch");
    let interrupt = AtomicBool::new(false);

    let outcome = {
        let remote = mirror
            .repo
            .find_default_remote(Direction::Fetch)
            .ok_or_else(|| Error::Gix("no default remote configured".into()))?
            .map_err(|e| Error::Gix(format!("find_default_remote: {e}")))?;

        let connection = remote
            .connect(Direction::Fetch)
            .map_err(|e| Error::Gix(format!("connect: {e}")))?;

        let prepared = connection
            .prepare_fetch(&mut progress, RefMapOptions::default())
            .map_err(|e| Error::Gix(format!("prepare_fetch: {e}")))?;

        prepared
            .receive(&mut progress, &interrupt)
            .map_err(|e| Error::Gix(format!("receive: {e}")))?
    };

    progress.finish();

    let mut updates = Vec::new();
    for mapping in &outcome.ref_map.mappings {
        let refname = mapping
            .remote
            .as_name()
            .map(|n| n.to_str_lossy().into_owned())
            .unwrap_or_default();
        if !refname.starts_with("refs/heads/") {
            continue;
        }
        let new_oid = mapping.remote.as_id().map(std::borrow::ToOwned::to_owned);
        let old_oid = mapping
            .local
            .as_ref()
            .and_then(|local| mirror.repo.find_reference(local.as_bstr()).ok())
            .and_then(|r| r.target().try_id().map(std::borrow::ToOwned::to_owned));
        if old_oid != new_oid {
            debug!(refname = %refname, ?old_oid, ?new_oid, "ref delta");
            updates.push(RefUpdate {
                refname,
                old_oid,
                new_oid,
            });
        }
    }

    info!(count = updates.len(), "fetch complete");
    Ok(updates)
}
