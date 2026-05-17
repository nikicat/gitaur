//! Persistent user configuration loaded from `~/.config/gitaur/config.toml`.

use crate::error::Result;
use crate::paths;
use crate::ui::ColorMode;
use serde::Deserialize;
use std::path::PathBuf;

pub mod defaults;

/// Runtime configuration. Defaults come from [`defaults::default_config`].
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where per-pkgbase worktrees live.
    pub build_dir: PathBuf,
    /// Git URL of the AUR mirror to clone.
    pub mirror_url: String,
    /// Worker count for parallel index builds.
    pub index_threads: usize,
    /// Re-fetch mirror if `index.bin` is older than this (used by no-arg run).
    pub refresh_max_age_secs: u64,
    /// `auto` | `always` | `never`.
    pub color: String,
    /// Path or name of the `makepkg` binary.
    pub makepkg_path: String,
    /// Default args passed to every `makepkg` invocation.
    pub makepkg_args: Vec<String>,
    /// `sudo` | `doas` | `run0` — used to elevate pacman calls.
    pub privilege_escalator: String,
    /// Include VCS pkgs (`-git`/`-svn`/…) in `-Syu` by default.
    pub devel: bool,
    /// `prompt` | `skip` | `always-show` — PKGBUILD review default.
    pub review_default: String,
}

impl Default for Config {
    fn default() -> Self {
        defaults::default_config()
    }
}

impl Config {
    /// Load from `config.toml` if present, else return defaults.
    pub fn load() -> Result<Self> {
        let path = paths::config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)?;
        let cfg: Config = toml::from_str(&text)?;
        Ok(cfg)
    }

    /// Translate the `color` string into a typed [`ColorMode`].
    pub fn color_mode(&self) -> ColorMode {
        match self.color.as_str() {
            "always" => ColorMode::Always,
            "never" => ColorMode::Never,
            _ => ColorMode::Auto,
        }
    }
}
