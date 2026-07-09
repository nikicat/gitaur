//! XDG-aware path helpers for aurox state and config files.

use crate::context_local;
use crate::names::PkgBase;
use std::path::PathBuf;

context_local! {
    /// Test-only override for [`state_dir`]. Lives in TLS so each test
    /// thread can install its own tempdir without leaking into siblings.
    /// Tests must drive this through the RAII helper in
    /// `crate::testing::ScopedStateRoot` rather than poking it directly — the
    /// guard restores the previous value on drop so failures don't strand the
    /// override. Declared via `context_local!` so it propagates onto spawned /
    /// rayon workers (a worker resolving `state_dir()` against the *real* dir
    /// instead of a test's tempdir was a test-isolation flake).
    pub(crate) static state_root: Option<PathBuf> = None;
}

/// Root for per-user mutable state (e.g. `$XDG_STATE_HOME/aurox`).
///
/// Honors the [`state_root`] override when present so tests can reroute state
/// into a tempdir without mutating process-wide env vars. Production callers
/// see it as `None` and fall through to the XDG path.
pub fn state_dir() -> PathBuf {
    if let Some(root) = state_root::get() {
        return root;
    }
    dirs::state_dir()
        .unwrap_or_else(|| dirs::home_dir().expect("home dir").join(".local/state"))
        .join("aurox")
}

/// Root for per-user config (e.g. `$XDG_CONFIG_HOME/aurox`).
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().expect("home dir").join(".config"))
        .join("aurox")
}

/// Path to the bare clone of the AUR mirror.
pub fn aur_repo_path() -> PathBuf {
    state_dir().join("aur")
}

/// Timestamp of the last successful AUR-mirror fetch.
///
/// Written by [`crate::mirror::cmd_refresh`] and read by the shell's `upgrade`
/// to throttle redundant network round-trips to within
/// [`crate::config::Config::refresh_max_age_secs`].
///
/// A dedicated stamp rather than an artifact mtime: gix writes no `FETCH_HEAD`,
/// `packed-refs` is only rewritten every few thousand fetches, and the index /
/// commit-graph are only touched when refs actually changed — so no existing
/// file reliably records "we contacted the mirror just now", least of all the
/// common no-op fetch. Lives under the state dir so the test override and XDG
/// rules apply uniformly.
pub fn fetch_stamp_path() -> PathBuf {
    state_dir().join("last-fetch")
}

/// Per-pkgbase worktree directory used during builds. `PathBuf::join`
/// consumes `&PkgBase` via its `AsRef<Path>` impl — no string downgrade.
pub fn pkg_worktree(pkgbase: &PkgBase) -> PathBuf {
    state_dir().join("pkgs").join(pkgbase)
}

/// Path to the mmap-loaded rkyv index file.
pub fn index_path() -> PathBuf {
    state_dir().join("index.bin")
}

/// Private pacman dbpath for aurox's rootless official-repo db sync.
///
/// See [`crate::pacman::sync`]. Holds a `sync/` dir of downloaded repo DBs and a
/// `local` symlink to the system localdb, so libalpm reads the real
/// installed-package set while available versions come from aurox's own fetch.
pub fn sync_db_path() -> PathBuf {
    state_dir().join("syncdb")
}

/// Path to `config.toml`.
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Directory holding per-invocation execution logs.
pub fn logs_dir() -> PathBuf {
    state_dir().join("logs")
}

/// Directory holding per-invocation Chrome/Perfetto span traces.
pub fn traces_dir() -> PathBuf {
    state_dir().join("traces")
}

/// `SQLite` store for cross-session build-time metrics — see
/// [`crate::build::metrics`].
///
/// Schema is intentionally minimal (one row per successful build, holding the
/// pkgbase, wall-time seconds, and a Unix-epoch-ms timestamp); everything else
/// (version, install date) is recoverable from pacman's localdb.
pub fn metrics_db_path() -> PathBuf {
    state_dir().join("metrics.db")
}

/// Persistent command history for the interactive shell (`aurox` REPL).
///
/// Lives under the state dir alongside logs/traces so it follows the same
/// XDG + test-override rules; rustyline loads it at session start and appends
/// on exit. See [`crate::cli::shell`].
pub fn shell_history_path() -> PathBuf {
    state_dir().join("shell_history")
}

/// Create the state directory tree if missing.
pub fn ensure_state_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir())?;
    std::fs::create_dir_all(state_dir().join("pkgs"))?;
    Ok(())
}
