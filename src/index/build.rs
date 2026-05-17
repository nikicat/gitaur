//! Parallel construction of [`IndexFile`] from a freshly-cloned mirror.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::index::schema::{IndexEntry, IndexFile};
use crate::index::srcinfo;
use crate::mirror::MirrorRepo;
use crate::ui;
use gix::ObjectId;
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tracing::{debug, info, instrument, warn};

/// Build a fresh index by scanning every `refs/heads/*` branch on the mirror.
#[instrument(skip(cfg, mirror))]
pub fn full_build(cfg: &Config, mirror: &MirrorRepo) -> Result<IndexFile> {
    let started = Instant::now();
    let refs: Vec<(String, ObjectId)> = collect_branches(&mirror.repo)?;
    info!(branches = refs.len(), "starting parallel index build");

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.index_threads)
        .build()
        .map_err(|e| Error::other(format!("rayon pool: {e}")))?;

    let pb = ui::bar_count(refs.len() as u64, "pkgs");
    let processed = AtomicU64::new(0);
    let skipped = AtomicU64::new(0);

    // gix::Repository is Send+Sync, so we can borrow it across rayon workers
    // directly. Each worker reuses the same handle.
    let repo_path: PathBuf = mirror.path.clone();
    let entries: Vec<IndexEntry> = pool.install(|| {
        refs.par_iter()
            .filter_map(|(branch, oid)| {
                let repo = match gix::open(&repo_path) {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(branch, error = %e, "reopen failed");
                        return None;
                    }
                };
                let r = parse_branch(&repo, branch, *oid);
                pb.inc(1);
                match r {
                    Ok(entry) => {
                        processed.fetch_add(1, Ordering::Relaxed);
                        Some(entry)
                    }
                    Err(e) => {
                        skipped.fetch_add(1, Ordering::Relaxed);
                        debug!(branch, error = %e, "branch skipped");
                        None
                    }
                }
            })
            .collect()
    });

    pb.finish_with_message("done");
    let mut entries = entries;
    entries.sort_by(|a, b| a.pkgbase.cmp(&b.pkgbase));

    let mirror_head = head_oid(&mirror.repo).unwrap_or([0u8; 20]);
    let idx = IndexFile {
        format_version: IndexFile::FORMAT_VERSION,
        mirror_head_oid: mirror_head,
        built_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        entries,
    };

    info!(
        entries = idx.entries.len(),
        skipped = skipped.load(Ordering::Relaxed),
        elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        "index build complete"
    );
    Ok(idx)
}

fn collect_branches(repo: &gix::Repository) -> Result<Vec<(String, ObjectId)>> {
    let refs = repo
        .references()
        .map_err(|e| Error::Gix(format!("references: {e}")))?;
    let iter = refs
        .prefixed("refs/heads/")
        .map_err(|e| Error::Gix(format!("prefixed: {e}")))?;
    let mut out = Vec::new();
    for r in iter {
        let mut r = r.map_err(|e| Error::Gix(format!("ref iter: {e}")))?;
        let name = r.name().shorten().to_string();
        let oid = r
            .peel_to_id()
            .map_err(|e| Error::Gix(format!("peel ref: {e}")))?
            .detach();
        out.push((name, oid));
    }
    Ok(out)
}

fn parse_branch(repo: &gix::Repository, branch: &str, oid: ObjectId) -> Result<IndexEntry> {
    let commit = repo
        .find_commit(oid)
        .map_err(|e| Error::Gix(format!("find_commit {oid}: {e}")))?;
    let tree = commit
        .tree()
        .map_err(|e| Error::Gix(format!("tree {oid}: {e}")))?;
    let entry = tree
        .find_entry(".SRCINFO")
        .ok_or_else(|| Error::SrcInfo(format!("no .SRCINFO on {branch}")))?;
    let blob_oid = entry.oid().to_owned();
    let blob = repo
        .find_object(blob_oid)
        .map_err(|e| Error::Gix(format!("find_blob {blob_oid}: {e}")))?;
    let text = std::str::from_utf8(blob.data.as_slice())
        .map_err(|e| Error::SrcInfo(format!("{branch}: invalid utf8: {e}")))?;
    let mut entry = srcinfo::parse(text)?;
    entry.commit_oid = oid_bytes(oid);
    entry.srcinfo_blob_oid = oid_bytes(blob_oid);
    Ok(entry)
}

fn oid_bytes(o: ObjectId) -> [u8; 20] {
    let mut b = [0u8; 20];
    b.copy_from_slice(o.as_slice());
    b
}

fn head_oid(repo: &gix::Repository) -> Option<[u8; 20]> {
    let head = repo.head_id().ok()?;
    Some(oid_bytes(head.detach()))
}
