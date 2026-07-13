//! System maintenance: on-disk state usage reporting and cache pruning.
//!
//! Backs the shell's `system` command group (`system show` / `system prune`).
//! Every category of state aurox writes lives under [`paths::state_dir`]; this
//! module knows which of them are *caches* — re-derivable from the AUR or the
//! pacman sync repos by a `refresh`/rebuild — and which are observational data
//! (build-time metrics, shell history) or rotation-capped diagnostics (logs,
//! traces) that a prune must never touch.

use crate::error::Result;
use crate::paths;
use crate::units::ByteSize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tracing::{debug, instrument};

/// One category of aurox's on-disk state, in `system show` display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateKind {
    /// The bare AUR git mirror (`aur/`) plus its fetch stamp.
    Mirror,
    /// The mmap-loaded rkyv package index (`index.bin`).
    Index,
    /// aurox's private pacman sync-db snapshot (`syncdb/`).
    SyncDb,
    /// Per-pkgbase build worktrees incl. built packages + source caches (`pkgs/`).
    Builds,
    /// Per-run execution logs (`logs/`, rotation-capped).
    Logs,
    /// Per-run span traces (`traces/`, rotation-capped).
    Traces,
    /// Cross-session build-time metrics (`metrics.db`) — not re-derivable.
    Metrics,
    /// Shell command history (`shell_history`) — not re-derivable.
    History,
}

/// Every category, in display order.
pub const ALL_KINDS: &[StateKind] = &[
    StateKind::Mirror,
    StateKind::Index,
    StateKind::SyncDb,
    StateKind::Builds,
    StateKind::Logs,
    StateKind::Traces,
    StateKind::Metrics,
    StateKind::History,
];

impl StateKind {
    /// Short row label for the `system show` table.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Mirror => "mirror",
            Self::Index => "index",
            Self::SyncDb => "syncdb",
            Self::Builds => "builds",
            Self::Logs => "logs",
            Self::Traces => "traces",
            Self::Metrics => "metrics",
            Self::History => "history",
        }
    }

    /// One-line "what lives here" for the `system show` table.
    pub const fn description(self) -> &'static str {
        match self {
            Self::Mirror => "AUR git mirror",
            Self::Index => "package search index",
            Self::SyncDb => "repo db snapshot",
            Self::Builds => "build worktrees + built packages",
            Self::Logs => "run logs (rotated)",
            Self::Traces => "run traces (rotated)",
            Self::Metrics => "build-time history",
            Self::History => "shell command history",
        }
    }

    /// Whether `system prune` deletes this category. True exactly for the
    /// caches — state a `refresh` (mirror, index, syncdb) or the next build
    /// (worktrees) re-derives. Diagnostics are already rotation-capped, and
    /// metrics/history are observational data nothing can reconstruct.
    pub const fn prunable(self) -> bool {
        matches!(
            self,
            Self::Mirror | Self::Index | Self::SyncDb | Self::Builds
        )
    }

    /// The filesystem paths this category owns. A `Vec` because a category
    /// can span several artifacts (the mirror clone plus its fetch stamp).
    pub fn roots(self) -> Vec<PathBuf> {
        match self {
            Self::Mirror => vec![paths::aur_repo_path(), paths::fetch_stamp_path()],
            Self::Index => vec![paths::index_path()],
            Self::SyncDb => vec![paths::sync_db_path()],
            Self::Builds => vec![paths::state_dir().join("pkgs")],
            Self::Logs => vec![paths::logs_dir()],
            Self::Traces => vec![paths::traces_dir()],
            Self::Metrics => vec![paths::metrics_db_path()],
            Self::History => vec![paths::shell_history_path()],
        }
    }

    /// Total apparent size of this category's paths right now.
    fn size(self) -> ByteSize {
        ByteSize::new(self.roots().iter().map(|p| path_size(p)).sum())
    }
}

/// One measured row of the `system show` report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub kind: StateKind,
    pub size: ByteSize,
}

/// Disk-usage report for `system show`: every category, measured.
#[derive(Debug)]
pub struct Report {
    /// The state root every category lives under, for the report header.
    pub root: PathBuf,
    /// Per-category sizes, in [`ALL_KINDS`] order.
    pub rows: Vec<Usage>,
}

impl Report {
    /// Sum over every category.
    pub fn total(&self) -> ByteSize {
        self.rows.iter().map(|u| u.size).sum()
    }

    /// Sum over the categories `system prune` would delete.
    pub fn prunable_total(&self) -> ByteSize {
        self.rows
            .iter()
            .filter(|u| u.kind.prunable())
            .map(|u| u.size)
            .sum()
    }
}

/// Measure every state category (missing paths count as zero).
#[instrument]
pub fn usage() -> Report {
    Report {
        root: paths::state_dir(),
        rows: ALL_KINDS
            .iter()
            .map(|&kind| Usage {
                kind,
                size: kind.size(),
            })
            .collect(),
    }
}

/// Delete every prunable category and recreate the state-dir skeleton.
///
/// Returns the bytes freed (measured just before deletion). Idempotent: a
/// missing path is "already pruned", not an error, so a failed prune can
/// simply be re-run.
#[instrument]
pub fn prune() -> Result<ByteSize> {
    let mut freed = 0;
    for kind in ALL_KINDS.iter().filter(|k| k.prunable()) {
        for root in kind.roots() {
            freed += path_size(&root);
            debug!(kind = kind.label(), path = %root.display(), "pruning");
            remove_path(&root)?;
        }
    }
    paths::ensure_state_dir()?;
    Ok(ByteSize::new(freed))
}

/// Remove a file, symlink, or directory tree; a missing path is a no-op.
///
/// Uses `symlink_metadata` so a symlink is removed as a link — never followed
/// into its target (the syncdb holds a `local` symlink into the real pacman
/// db, which must survive a prune untouched).
fn remove_path(path: &Path) -> io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Apparent size in bytes of a file or directory tree; `0` when missing.
///
/// Symlinks contribute their own (link) size and are never followed — both to
/// stay honest about what a prune would free and so the syncdb's `local`
/// symlink doesn't drag the system pacman db into the count. Unreadable
/// entries are skipped (best-effort reporting), traced at debug.
fn path_size(path: &Path) -> u64 {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                debug!(path = %path.display(), error = %e, "skipping unreadable path");
            }
            return 0;
        }
    };
    if !meta.is_dir() {
        return meta.len();
    }
    let entries = match fs::read_dir(path) {
        Ok(it) => it,
        Err(e) => {
            debug!(path = %path.display(), error = %e, "skipping unreadable dir");
            return 0;
        }
    };
    entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            // `DirEntry::metadata` doesn't traverse symlinks — a link counts
            // as itself, its target stays out of the sum.
            let meta = entry.metadata().ok()?;
            Some(if meta.is_dir() {
                path_size(&entry.path())
            } else {
                meta.len()
            })
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::ScopedStateRoot;
    use std::os::unix::fs::symlink;

    /// Lay out a miniature state dir with every category populated, plus an
    /// out-of-tree directory the syncdb links to (standing in for the real
    /// `/var/lib/pacman/local`).
    fn seed(root: &Path) -> PathBuf {
        for dir in ["aur/objects", "syncdb/sync", "pkgs/yay", "logs", "traces"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(root.join("aur/objects/pack"), [0u8; 300]).unwrap();
        fs::write(root.join("index.bin"), [0u8; 200]).unwrap();
        fs::write(root.join("syncdb/sync/core.db"), [0u8; 100]).unwrap();
        fs::write(root.join("pkgs/yay/yay.pkg.tar.zst"), [0u8; 50]).unwrap();
        fs::write(root.join("last-fetch"), [0u8; 1]).unwrap();
        fs::write(root.join("logs/aurox-1.log"), [0u8; 10]).unwrap();
        fs::write(root.join("traces/aurox-1.json"), [0u8; 20]).unwrap();
        fs::write(root.join("metrics.db"), [0u8; 30]).unwrap();
        fs::write(root.join("shell_history"), [0u8; 40]).unwrap();

        // The stand-in system localdb, outside the state root.
        let localdb = root.parent().unwrap().join("system-localdb");
        fs::create_dir_all(&localdb).unwrap();
        fs::write(localdb.join("huge.db"), vec![0u8; 100_000]).unwrap();
        symlink(&localdb, root.join("syncdb/local")).unwrap();
        localdb
    }

    fn size_of(report: &Report, kind: StateKind) -> u64 {
        report
            .rows
            .iter()
            .find(|u| u.kind == kind)
            .unwrap()
            .size
            .bytes()
    }

    #[test]
    fn usage_measures_each_category_without_following_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        fs::create_dir_all(&root).unwrap();
        let _guard = ScopedStateRoot::new(root.clone());
        seed(&root);

        let report = usage();
        assert_eq!(report.root, root);
        assert_eq!(size_of(&report, StateKind::Mirror), 300 + 1); // clone + stamp
        assert_eq!(size_of(&report, StateKind::Index), 200);
        // The syncdb sum must be the db file plus the `local` *link* itself —
        // never the 100 kB target it points at.
        let syncdb = size_of(&report, StateKind::SyncDb);
        assert!(
            (100..1000).contains(&syncdb),
            "syncdb size {syncdb} should exclude the symlink target"
        );
        assert_eq!(size_of(&report, StateKind::Builds), 50);
        assert_eq!(size_of(&report, StateKind::Metrics), 30);
        assert_eq!(size_of(&report, StateKind::History), 40);
        assert_eq!(
            report.prunable_total().bytes(),
            301 + 200 + syncdb + 50,
            "prunable = mirror + index + syncdb + builds"
        );
    }

    #[test]
    fn prune_deletes_caches_keeps_data_and_never_reaches_through_the_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        fs::create_dir_all(&root).unwrap();
        let _guard = ScopedStateRoot::new(root.clone());
        let localdb = seed(&root);
        let expected = usage().prunable_total();

        let freed = prune().unwrap();
        assert_eq!(
            freed, expected,
            "freed must match the reported prunable total"
        );

        // Caches gone…
        for gone in ["aur", "index.bin", "syncdb", "last-fetch"] {
            assert!(!root.join(gone).exists(), "{gone} should be pruned");
        }
        // …observational data + diagnostics kept…
        for kept in ["metrics.db", "shell_history", "logs", "traces"] {
            assert!(root.join(kept).exists(), "{kept} should survive a prune");
        }
        // …the symlink target untouched, and the skeleton recreated for the
        // next build.
        assert!(
            localdb.join("huge.db").exists(),
            "prune must not follow syncdb/local"
        );
        assert!(root.join("pkgs").is_dir(), "pkgs skeleton recreated");
    }

    #[test]
    fn prune_of_an_empty_state_dir_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        fs::create_dir_all(&root).unwrap();
        let _guard = ScopedStateRoot::new(root);
        assert_eq!(prune().unwrap(), ByteSize::ZERO);
    }
}
