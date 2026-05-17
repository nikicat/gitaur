//! XDG-aware path helpers for gitaur state and config files.

use std::path::PathBuf;

/// Root for per-user mutable state (e.g. `$XDG_STATE_HOME/gitaur`).
pub fn state_dir() -> PathBuf {
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

/// Create the state directory tree if missing.
pub fn ensure_state_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir())?;
    std::fs::create_dir_all(state_dir().join("pkgs"))?;
    Ok(())
}
