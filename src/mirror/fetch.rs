//! Incremental fetch of the AUR mirror via gix.
//!
//! Returns a per-ref delta vector ([`RefUpdate`]) compatible with the rest of
//! the index pipeline.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::mirror::{MirrorRepo, boxed_http_options};
use crate::ui::GixProgress;
use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::refs::TargetRef;
use gix::remote::fetch::refs::update::{Mode, Outcome as UpdateOutcome};
use gix::remote::fetch::{Status, refmap::Mapping};
use gix::remote::{Direction, ref_map::Options as RefMapOptions};
use std::sync::atomic::AtomicBool;
use std::time::Instant;
use tracing::{debug, info, info_span, instrument};

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
#[instrument(skip(cfg, mirror))]
pub fn incremental_fetch(cfg: &Config, mirror: &MirrorRepo) -> Result<Vec<RefUpdate>> {
    let mut progress = GixProgress::new("fetch");
    let interrupt = AtomicBool::new(false);

    let outcome = {
        let remote = mirror
            .repo
            .find_default_remote(Direction::Fetch)
            .ok_or_else(|| Error::Gix("no default remote configured".into()))?
            .map_err(|e| Error::Gix(format!("find_default_remote: {e}")))?;

        let mut connection = remote
            .connect(Direction::Fetch)
            .map_err(|e| Error::Gix(format!("connect: {e}")))?;
        connection.set_transport_options(boxed_http_options(cfg));

        let prepared = {
            let _span = info_span!("prepare_fetch").entered();
            debug!("preparing fetch: handshake + list refs against remote");
            let t_prepare = Instant::now();
            let prepared = connection
                .prepare_fetch(&mut progress, RefMapOptions::default())
                .map_err(|e| Error::Gix(format!("prepare_fetch: {e}")))?;
            debug!(
                elapsed_ms = u64::try_from(t_prepare.elapsed().as_millis()).unwrap_or(u64::MAX),
                "prepare_fetch returned (ref advertisement complete)"
            );
            prepared
        };

        // gix leaves its phase label stuck on `list refs` through the silent
        // have-set build below (it doesn't `set_name` again until negotiation),
        // so close the stale span here. Otherwise the gap is mislabeled as
        // `list refs` instead of falling under the `receive` span.
        progress.clear_phase();

        // The next ~30–60s on a large mirror are gix-internal and silent:
        //   1. build local "have" set from existing refs (silent ~20s on AUR)
        //   2. negotiate (visible — `set_name=negotiate (round N)`)
        //   3. receive + index pack (visible — `read pack`, `create index file`)
        //   4. update refs / write pack manifest (silent ~15s on AUR)
        // The `receive` span makes the whole thing one timed block; the silent
        // prelude (1) shows as the gap before gix's first `negotiate` sub-span.
        let _span = info_span!("receive").entered();
        debug!("entering receive: build have-set, negotiate, fetch pack, update refs");
        let t_receive = Instant::now();
        let outcome = prepared
            .receive(&mut progress, &interrupt)
            .map_err(|e| Error::Gix(format!("receive: {e}")))?;
        debug!(
            elapsed_ms = u64::try_from(t_receive.elapsed().as_millis()).unwrap_or(u64::MAX),
            "receive returned (pack written, refs negotiated)"
        );
        outcome
    };

    progress.finish();

    debug!(
        mappings = outcome.ref_map.mappings.len(),
        "extracting ref deltas from fetch outcome"
    );
    let t_extract = Instant::now();
    let update_refs = match &outcome.status {
        Status::Change { update_refs, .. } | Status::NoPackReceived { update_refs, .. } => {
            update_refs
        }
    };
    let updates = extract_branch_updates(&outcome.ref_map.mappings, update_refs);
    debug!(
        updates = updates.len(),
        elapsed_ms = u64::try_from(t_extract.elapsed().as_millis()).unwrap_or(u64::MAX),
        "extracted ref deltas"
    );

    info!(count = updates.len(), "fetch complete");
    Ok(updates)
}

/// Walk gix's own update report (no local ref lookups).
///
/// `update_refs.updates[i]` corresponds 1:1 with `mappings[i]`. We keep only
/// `refs/heads/*` mappings where the mode is an actual change.
fn extract_branch_updates(mappings: &[Mapping], update_refs: &UpdateOutcome) -> Vec<RefUpdate> {
    update_refs
        .updates
        .iter()
        .zip(mappings.iter())
        .filter_map(|(update, mapping)| {
            match update.mode {
                Mode::New | Mode::FastForward | Mode::Forced => {}
                _ => return None,
            }
            let refname = mapping.remote.as_name()?.to_str_lossy().into_owned();
            if !refname.starts_with("refs/heads/") {
                return None;
            }
            let edit = update_refs.edits.get(update.edit_index?)?;
            let new_oid = match edit.change.new_value() {
                Some(TargetRef::Object(oid)) => Some(oid.to_owned()),
                _ => None,
            };
            // For Mode::New, `previous_value()` is set to the new value as a
            // sentinel — there's no real prior value. Don't surface it.
            let old_oid = if matches!(update.mode, Mode::New) {
                None
            } else {
                match edit.change.previous_value() {
                    Some(TargetRef::Object(oid)) => Some(oid.to_owned()),
                    _ => None,
                }
            };
            debug!(refname = %refname, ?old_oid, ?new_oid, mode = ?update.mode, "ref delta");
            Some(RefUpdate {
                refname,
                old_oid,
                new_oid,
            })
        })
        .collect()
}
