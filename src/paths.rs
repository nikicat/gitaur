//! XDG-aware path helpers for gitaur state and config files.

use std::cell::RefCell;
use std::path::PathBuf;

thread_local! {
    /// Test-only override for [`state_dir`]. Lives in TLS so each test
    /// thread can install its own tempdir without leaking into siblings.
    /// Tests must drive this through the RAII helper in
    /// `crate::testing::ScopedStateRoot` rather than poking the TLS
    /// directly — the guard restores the previous value on drop so
    /// failures don't strand the override.
    pub(crate) static STATE_ROOT_OVERRIDE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Root for per-user mutable state (e.g. `$XDG_STATE_HOME/gitaur`).
///
/// Honors the [`STATE_ROOT_OVERRIDE`] TLS when present so tests can reroute
/// state into a tempdir without mutating process-wide env vars. Production
/// callers see the TLS as `None` and fall through to the XDG path.
pub fn state_dir() -> PathBuf {
    if let Some(root) = STATE_ROOT_OVERRIDE.with(|c| c.borrow().clone()) {
        return root;
    }
    dirs::state_dir()
        .unwrap_or_else(|| dirs::home_dir().expect("home dir").join(".local/state"))
        .join("gitaur")
}

/// Root for per-user config (e.g. `$XDG_CONFIG_HOME/gitaur`).
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().expect("home dir").join(".config"))
        .join("gitaur")
}

/// Path to the bare clone of the AUR mirror.
pub fn aur_repo_path() -> PathBuf {
    state_dir().join("aur")
}

/// Per-pkgbase worktree directory used during builds.
pub fn pkg_worktree(name: &str) -> PathBuf {
    state_dir().join("pkgs").join(name)
}

/// Path to the mmap-loaded rkyv index file.
pub fn index_path() -> PathBuf {
    state_dir().join("index.bin")
}

/// Path to the `SQLite` build state database.
pub fn state_db_path() -> PathBuf {
    state_dir().join("state.db")
}

/// Path to `config.toml`.
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// Directory holding per-invocation execution logs.
pub fn logs_dir() -> PathBuf {
    state_dir().join("logs")
}

/// Create the state directory tree if missing.
pub fn ensure_state_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir())?;
    std::fs::create_dir_all(state_dir().join("pkgs"))?;
    Ok(())
}

/// Create the logs directory if missing.
pub fn ensure_logs_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(logs_dir())
}
