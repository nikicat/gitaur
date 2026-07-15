//! Bare clone of the AUR mirror plus per-pkgbase build-directory materialization.
//!
//! Built on [`gix`] (gitoxide), pure Rust. No subprocess, no libgit2.
//! Per-pkgbase directories are *materialized* from the bare repo's tree
//! objects rather than created via `git worktree add` — aurox owns those
//! directories, so a plain checkout is sufficient.

use crate::config::Config;
use crate::context;
use crate::error::{Error, Result};
use crate::git;
use crate::index;
use crate::pacman::sync::{self, SyncOutcome};
use crate::paths;
use crate::ui;
use gix::protocol::transport::client::blocking_io::http;
use indicatif::MultiProgress;
use std::any::Any;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::debug;

pub mod clone;
mod consent;
pub mod fetch;
pub mod sideband;
pub mod worktree;

use consent::AurAction;
pub use consent::{RefreshOutcome, RefreshReason, SkipCause};

/// Build the `http::Options` payload gix's curl transport downcasts in its
/// `configure()` hook. Sets `lowSpeedLimit=1`, `lowSpeedTime=idle_timeout_secs`
/// so the connection aborts after `idle_timeout_secs` of <1 byte/s — i.e., true
/// silence from the remote, not a total deadline (0 disables the guard).
/// Callers pick the window per phase: incremental fetches pass
/// `cfg.mirror_idle_timeout_secs`, the bootstrap clone the far larger
/// `cfg.bootstrap_idle_timeout_secs` (see its doc for why). `download_progress`
/// is the counter the backend adds each received body chunk to, driving the
/// UI's `network` throughput row (the only live signal during the
/// otherwise-silent ls-refs advertisement).
pub(crate) fn http_transport_options(
    idle_timeout_secs: u64,
    download_progress: Arc<AtomicU64>,
) -> http::Options {
    let mut opts = http::Options::default();
    if idle_timeout_secs > 0 {
        opts.low_speed_limit_bytes_per_second = 1;
        opts.low_speed_time_seconds = idle_timeout_secs;
    }
    opts.download_progress = Some(download_progress);
    opts
}

/// `set_transport_options` wants `Box<dyn Any>`; wrap once at the call site.
pub(crate) fn boxed_http_options(
    idle_timeout_secs: u64,
    download_progress: Arc<AtomicU64>,
) -> Box<dyn Any + Send + Sync> {
    Box::new(http_transport_options(idle_timeout_secs, download_progress))
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
            gix::open(path).map_err(|e| Error::gix(format_args!("open {}", path.display()), e))?;
        Ok(Self {
            path: path.to_path_buf(),
            repo,
        })
    }

    /// Refresh the mirror's commit-graph so the *next* fetch's negotiation can
    /// read commit times from an mmap'd file instead of inflating every commit
    /// from the pack (the dominant remaining cost of building the have-set).
    ///
    /// `new_commits` is forwarded to [`crate::git::write_commit_graph`]:
    /// `Some(tips)` for an incremental fetch (only those tips' closure is
    /// graphed), `None` for a fresh clone / full rebuild (walk every ref).
    /// Best-effort — see [`crate::git::write_commit_graph`].
    pub fn write_commit_graph(&self, new_commits: Option<&[gix::ObjectId]>) {
        git::write_commit_graph(&self.path, new_commits);
    }
}

/// Refresh aurox's databases: the AUR mirror — subject to the bootstrap
/// consent gate — and, unless [`Config::check_repo_updates`] is off, the
/// official-repo sync DBs in parallel.
///
/// Both halves draw into one shared [`MultiProgress`] so the AUR fetch rows and
/// the per-repo db-download rows line up in a single display. The repo sync is
/// best-effort: a failure there is reported as a warning and never fails the
/// AUR refresh (whose result is what this returns).
///
/// `reason` says who asked (see [`RefreshReason`]): [`RefreshReason::ForceReclone`]
/// (`aurox -Syy`) blows away the existing bare clone and re-bootstraps from
/// scratch, and the reason also picks how consent for a needed bootstrap is
/// obtained — the ~2 GiB clone never starts without a yes. A decline (or
/// `aur = false` in config.toml) still refreshes the sync DBs and returns
/// [`RefreshOutcome::AurSkipped`] so callers can hint at what was skipped.
pub fn cmd_refresh(cfg: &Config, reason: RefreshReason) -> Result<RefreshOutcome> {
    // Resolve consent before the progress display exists: dialoguer and
    // indicatif both draw on the terminal, and a prompt under live progress
    // rows gets clobbered by redraws.
    let action = consent::plan(cfg, reason)?;
    let mp = MultiProgress::new();
    let aur = if cfg.check_repo_updates {
        // Scoped thread: the official-repo db sync (libalpm download) overlaps
        // the network-bound AUR fetch. It borrows `cfg`/`mp` for the scope and
        // draws its own rows into the shared display.
        context::scope(|s| {
            let repo = s.spawn(|| sync::refresh_sync_db(&mp));
            let aur = run_aur_action(cfg, action, &mp);
            report_repo_sync(repo.join());
            aur
        })
    } else {
        run_aur_action(cfg, action, &mp)
    };
    // Backstop: wipe any progress rows a mid-download error may have left
    // (each row normally clears itself on completion).
    mp.clear().ok();
    // A successful refresh stamps the TTL that the shell's `upgrade` honours.
    // Deliberately stamped on `AurSkipped` too: a declined bootstrap must not
    // re-prompt on every TTL-driven `upgrade` within the window.
    if aur.is_ok() {
        record_fetch_stamp();
    }
    aur
}

/// Record "the mirror was fetched just now" so the shell's `upgrade` can skip a
/// redundant fetch within [`Config::refresh_max_age_secs`]. Best-effort: a write
/// failure just means the next `upgrade` re-fetches (the pre-TTL behaviour),
/// never an error. See [`paths::fetch_stamp_path`] for why this is a stamp file
/// rather than an artifact mtime.
fn record_fetch_stamp() {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    if let Err(e) = std::fs::write(paths::fetch_stamp_path(), secs.to_string()) {
        debug!(error = %e, "record AUR fetch stamp");
    }
}

/// How long ago the AUR mirror was last fetched, per the stamp
/// [`record_fetch_stamp`] writes. `None` when it was never fetched (no stamp) or
/// the stamp is unreadable/garbled — the caller then treats the mirror as stale
/// and fetches, matching the always-fetch behaviour that predated the TTL. A
/// future stamp (the clock moved backwards) reads as a zero age rather than
/// re-fetching on every `upgrade`.
pub(crate) fn last_fetch_age() -> Option<Duration> {
    let raw = std::fs::read_to_string(paths::fetch_stamp_path()).ok()?;
    let secs: u64 = raw.trim().parse().ok()?;
    let stamped = UNIX_EPOCH + Duration::from_secs(secs);
    Some(
        SystemTime::now()
            .duration_since(stamped)
            .unwrap_or(Duration::ZERO),
    )
}

/// Surface the parallel repo-db sync's outcome once the shared progress display
/// is torn down. Best-effort — every failure mode is a warning, never fatal.
fn report_repo_sync(joined: std::thread::Result<Result<SyncOutcome>>) {
    match joined {
        Ok(Ok(SyncOutcome::Refreshed)) => ui::note("official package databases refreshed"),
        Ok(Ok(SyncOutcome::AlreadyCurrent)) => ui::note("official package databases up to date"),
        // Ctrl+C while waiting out a concurrent refresh's advisory lock — a
        // deliberate skip, not a failure.
        Ok(Err(Error::Interrupted)) => ui::note("official-repo refresh skipped"),
        Ok(Err(e)) => ui::warn(&format!("official-repo refresh failed: {e}")),
        Err(_) => ui::warn("official-repo refresh thread panicked"),
    }
}

/// Execute the consented AUR half of one refresh, drawing progress into the
/// shared `mp`.
fn run_aur_action(cfg: &Config, action: AurAction, mp: &MultiProgress) -> Result<RefreshOutcome> {
    match action {
        AurAction::Skip(cause) => Ok(RefreshOutcome::AurSkipped(cause)),
        AurAction::Bootstrap(_) => {
            bootstrap_aur(cfg, mp)?;
            Ok(RefreshOutcome::Refreshed)
        }
        AurAction::Fetch => {
            fetch_aur(cfg, mp)?;
            Ok(RefreshOutcome::Refreshed)
        }
    }
}

/// Full bootstrap: wipe whatever is on disk (an interrupted clone or a
/// force-recloned mirror), clone from scratch, build the index, seed the
/// commit-graph. Consent — including for the wipe — was already obtained in
/// [`consent::plan`], which also announced what is about to happen.
fn bootstrap_aur(cfg: &Config, mp: &MultiProgress) -> Result<()> {
    let path = paths::aur_repo_path();
    if path.exists() {
        std::fs::remove_dir_all(&path)?;
    }
    clone::bootstrap_clone(cfg, &path, mp)?;
    ui::info("building index");
    let mirror = MirrorRepo::open(&path)?;
    let idx = index::build::full_build(cfg, &mirror)?;
    index::save(&idx, &paths::index_path())?;
    ui::info("index built");
    // Seed the commit-graph so the first incremental `-Sy` negotiates fast.
    // Fresh clone: no delta, so walk every ref (`--reachable`).
    mirror.write_commit_graph(None);
    Ok(())
}

/// Fetch AUR mirror updates and incrementally refresh the on-disk index,
/// drawing progress into the shared `mp`.
fn fetch_aur(cfg: &Config, mp: &MultiProgress) -> Result<()> {
    let path = paths::aur_repo_path();
    ui::info("refreshing AUR mirror");
    let mirror = MirrorRepo::open(&path)?;
    let idx_path = paths::index_path();

    // The fetch is network-bound and the index load is local file I/O against
    // a different file, so run them concurrently: the ~0.5s load disappears
    // under the multi-second fetch. A scoped thread lets the loader borrow
    // `&idx_path` without an `Arc`; the fetch keeps the foreground (and its
    // progress UI) on this thread.
    //
    // A failed load (rkyv validation, schema mismatch after a aurox upgrade,
    // etc.) is **recovered from in-place** by falling back to a full rebuild
    // below — otherwise the user would be stuck in a loop where `-Sy` errors
    // out before it can rebuild.
    let (updates, existing) = context::scope(|s| {
        let loader = s.spawn(|| {
            if !idx_path.exists() {
                return None;
            }
            match index::load(&idx_path) {
                Ok(idx) => Some(idx),
                Err(e) => {
                    // Expected after a aurox upgrade bumps the schema: the
                    // rebuild below is announced by "building index"/"index
                    // built", and on the resync path `load_or_resync` has
                    // already told the user why. So this is a trace, not a
                    // user-facing warning.
                    debug!(error = %e, "existing index unreadable; rebuilding from scratch");
                    None
                }
            }
        });
        let updates = fetch::incremental_fetch(cfg, &mirror, mp)?;
        let existing = loader.join().expect("index loader thread panicked");
        Ok::<_, Error>((updates, existing))
    })?;

    match existing {
        Some(mut idx) if !updates.is_empty() => {
            index::update::incremental_update(&mirror, &updates, &mut idx)?;
            index::save(&idx, &idx_path)?;
            ui::note(&format!("{} ref(s) updated", updates.len()));
            // New commits arrived; fold them into the commit-graph for next
            // time. Feed just the fetched tips (`--stdin-commits`) so git
            // graphs their closure instead of re-walking all ~155k refs.
            let tips: Vec<gix::ObjectId> = updates.iter().filter_map(|u| u.new_oid).collect();
            mirror.write_commit_graph(Some(&tips));
        }
        Some(_) => {
            // Nothing changed on the mirror, so the commit-graph is still current.
            ui::note("no ref updates");
        }
        None => {
            ui::info("building index");
            let idx = index::build::full_build(cfg, &mirror)?;
            index::save(&idx, &idx_path)?;
            ui::info("index built");
            // Full rebuild: graph the whole history (`--reachable`).
            mirror.write_commit_graph(None);
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
