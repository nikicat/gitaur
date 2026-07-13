//! Incremental update of [`IndexFile`] driven by mirror fetch deltas.

use crate::error::{Error, Result};
use crate::index::schema::{IndexEntry, IndexFile};
use crate::index::srcinfo;
use crate::mirror::MirrorRepo;
use crate::mirror::fetch::RefUpdate;
use crate::names::PkgBase;
use crate::units::UnixTime;
use gix::ObjectId;
use std::collections::HashMap;
use tracing::{debug, info, instrument, warn};

/// Apply each [`RefUpdate`] to the in-memory index, in place.
#[instrument(skip(mirror, idx), fields(updates = updates.len()))]
pub fn incremental_update(
    mirror: &MirrorRepo,
    updates: &[RefUpdate],
    idx: &mut IndexFile,
) -> Result<()> {
    let mut by_base: HashMap<PkgBase, usize> = idx
        .entries
        .iter()
        .enumerate()
        .map(|(i, e)| (e.pkgbase.clone(), i))
        .collect();

    let mut deleted = 0u64;
    let mut upserted = 0u64;

    for u in updates {
        let branch = u
            .refname
            .strip_prefix("refs/heads/")
            .unwrap_or(&u.refname)
            .to_owned();

        // Ref deleted: drop the entry if we had one. `by_base`'s `Borrow<str>`
        // impl lets us look up by the raw branch name without allocating a
        // PkgBase just for the probe.
        let Some(new_oid) = u.new_oid else {
            if let Some(i) = by_base.remove(branch.as_str()) {
                idx.entries.swap_remove(i);
                deleted += 1;
                by_base = idx
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| (e.pkgbase.clone(), i))
                    .collect();
            }
            continue;
        };

        match parse_branch(mirror, &branch, new_oid) {
            Ok(entry) => {
                upserted += 1;
                if let Some(i) = by_base.get(&entry.pkgbase) {
                    idx.entries[*i] = entry;
                } else {
                    by_base.insert(entry.pkgbase.clone(), idx.entries.len());
                    idx.entries.push(entry);
                }
            }
            Err(e) => warn!(branch, error = %e, "skipping branch with bad .SRCINFO"),
        }
    }

    idx.entries.sort_by(|a, b| a.pkgbase.cmp(&b.pkgbase));
    info!(deleted, upserted, "incremental update applied");
    debug!(total = idx.entries.len(), "index size after update");
    Ok(())
}

fn parse_branch(mirror: &MirrorRepo, branch: &str, oid: ObjectId) -> Result<IndexEntry> {
    let commit = mirror
        .repo
        .find_commit(oid)
        .map_err(|e| Error::gix(format_args!("find_commit {oid}"), e))?;
    let tree = commit
        .tree()
        .map_err(|e| Error::gix(format_args!("tree {oid}"), e))?;
    let te = tree
        .find_entry(".SRCINFO")
        .ok_or_else(|| Error::SrcInfo(format!("{branch}: no .SRCINFO")))?;
    let blob_oid = te.oid().to_owned();
    let blob = mirror
        .repo
        .find_object(blob_oid)
        .map_err(|e| Error::gix(format_args!("find_blob {blob_oid}"), e))?;
    let text = std::str::from_utf8(blob.data.as_slice())
        .map_err(|e| Error::SrcInfo(format!("{branch}: utf8: {e}")))?;
    let mut entry = srcinfo::parse(text)?;
    entry.commit_oid = oid_bytes(oid);
    entry.srcinfo_blob_oid = oid_bytes(blob_oid);
    // Same stamp as the full build's `parse_branch` — an omission here left
    // incrementally-updated entries at the `0` sentinel, so a freshly-pushed
    // package sorted *oldest* in the picker until the next full rebuild.
    entry.commit_time = UnixTime::new(commit.time().map(|t| t.seconds).unwrap_or_default());
    Ok(entry)
}

fn oid_bytes(o: ObjectId) -> [u8; 20] {
    let mut b = [0u8; 20];
    b.copy_from_slice(o.as_slice());
    b
}
