//! Parallel construction of [`IndexFile`] from a freshly-cloned mirror.

use crate::config::Config;
use crate::context;
use crate::error::{Error, Result};
use crate::index::schema::{IndexEntry, IndexFile};
use crate::index::srcinfo;
use crate::mirror::MirrorRepo;
use crate::ui;
use gix::ObjectId;
use rayon::prelude::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tracing::{debug, info, instrument, warn};

/// Regression guardrail for [`full_build`].
///
/// `gix::Repository` is `Send` but **not** `Sync` (interior `RefCell`s for
/// object caches), so rayon workers can't share a single `&Repository`. The
/// correct pattern is `par_iter().map_init(|| mirror.repo.clone(), ...)`: one
/// cheap structural clone per worker thread (shared `Arc`'d object DB + refs),
/// reused across every branch that worker pulls. Re-`gix::open`-ing the bare
/// repo on every branch instead reparses config, rescans refs, and re-discovers
/// alternates *N* times — for a 150k-branch mirror that dominates wall time
/// (observed: ~2.2ms/branch ⇒ 5+ minutes).
///
/// Any worker-side `gix::open` call must bump this counter so the integration
/// test in `tests/build_worker_shares_repo.rs` catches the regression. Pure
/// instrumentation — exposed via `#[doc(hidden)] pub` so tests in `tests/` can
/// reach it (the lib is built without `--test` for integration tests, so
/// `#[cfg(test)]` would be invisible). Not part of the public API.
#[doc(hidden)]
pub static WORKER_REPO_OPENS: AtomicU64 = AtomicU64::new(0);

/// Build a fresh index by scanning every `refs/heads/*` branch on the mirror.
#[instrument(skip(cfg, mirror))]
pub fn full_build(cfg: &Config, mirror: &MirrorRepo) -> Result<IndexFile> {
    let started = Instant::now();
    let refs: Vec<(String, ObjectId)> = collect_branches(&mirror.repo)?;
    info!(branches = refs.len(), "starting parallel index build");

    // `context::thread_pool` so each worker inherits the caller's context
    // (state-dir override, run options); a bare rayon pool worker would start
    // from the defaults.
    let pool = context::thread_pool(cfg.index_threads)
        .map_err(|e| Error::other(format!("rayon pool: {e}")))?;

    let pb = ui::bar_count(refs.len() as u64, "pkgs");
    let processed = AtomicU64::new(0);
    let skipped = AtomicU64::new(0);

    // gix::Repository is Send but NOT Sync (interior RefCell caches), so rayon
    // workers can't share `&mirror.repo`. Each worker gets its own cheap clone
    // (shares the underlying Arc'd object DB + refs; only the per-thread
    // RefCell caches are fresh). `map_init` calls the init closure lazily once
    // per worker thread, so the Mutex is contended only `cfg.index_threads`
    // times total (not per branch) — overhead is sub-millisecond. Don't
    // replace this with a per-iter `gix::open(&path)`: it reparses config +
    // scans refs per branch and dominates wall time (see WORKER_REPO_OPENS).
    let repo_source = Mutex::new(mirror.repo.clone());
    let mut entries: Vec<IndexEntry> = pool.install(|| {
        refs.par_iter()
            .map_init(
                || repo_source.lock().expect("repo_source poisoned").clone(),
                |repo, (branch, oid)| {
                    let r = parse_branch(repo, branch, *oid);
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
                },
            )
            .flatten()
            .collect()
    });

    pb.finish_with_message("done");
    entries.sort_by(|a, b| a.pkgbase.cmp(&b.pkgbase));

    // `[0u8; 20]` doubles as the "no head yet" sentinel that `IndexFile::empty()`
    // uses, so a freshly-cloned mirror with no commits maps cleanly to it. But
    // it would also mask a corrupt/unreadable HEAD as "empty mirror" — log the
    // fallback so post-mortems can tell the two apart.
    let mirror_head = head_oid(&mirror.repo).unwrap_or_else(|| {
        debug!("mirror HEAD unreadable; writing zero OID sentinel into index");
        [0u8; 20]
    });
    let idx = IndexFile {
        format_version: IndexFile::FORMAT_VERSION,
        mirror_head_oid: mirror_head,
        built_at_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
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
    let refs = repo.references().map_err(|e| Error::gix("references", e))?;
    let iter = refs
        .prefixed("refs/heads/")
        .map_err(|e| Error::gix("prefixed", e))?;
    let mut out = Vec::new();
    for r in iter {
        let mut r = r.map_err(|e| Error::gix("ref iter", e))?;
        let name = r.name().shorten().to_string();
        let oid = r
            .peel_to_id()
            .map_err(|e| Error::gix("peel ref", e))?
            .detach();
        out.push((name, oid));
    }
    Ok(out)
}

fn parse_branch(repo: &gix::Repository, branch: &str, oid: ObjectId) -> Result<IndexEntry> {
    let commit = repo
        .find_commit(oid)
        .map_err(|e| Error::gix(format_args!("find_commit {oid}"), e))?;
    let tree = commit
        .tree()
        .map_err(|e| Error::gix(format_args!("tree {oid}"), e))?;
    let entry = tree
        .find_entry(".SRCINFO")
        .ok_or_else(|| Error::SrcInfo(format!("no .SRCINFO on {branch}")))?;
    let blob_oid = entry.oid().to_owned();
    let blob = repo
        .find_object(blob_oid)
        .map_err(|e| Error::gix(format_args!("find_blob {blob_oid}"), e))?;
    let text = std::str::from_utf8(blob.data.as_slice())
        .map_err(|e| Error::SrcInfo(format!("{branch}: invalid utf8: {e}")))?;
    let mut entry = srcinfo::parse(text)?;
    entry.commit_oid = oid_bytes(oid);
    entry.srcinfo_blob_oid = oid_bytes(blob_oid);
    // Committer time of the branch tip — what `aurox <term>` sorts the AUR
    // hits on. A branch whose commit time can't be decoded keeps the `0`
    // default (sorts oldest), rather than failing the whole branch parse.
    entry.commit_time_unix = commit.time().map(|t| t.seconds).unwrap_or_default();
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
