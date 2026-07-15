//! Persistent user configuration loaded from `~/.config/aurox/config.toml`.

use crate::cli::shell::cart::AurApproval;
use crate::error::Result;
use crate::paths;
use crate::ui::ColorMode;
use optfield::optfield;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod defaults;

/// Resolved runtime configuration.
///
/// Defaults come from [`defaults::default_config`]; the on-disk schema is
/// the optfield-generated [`ConfigFile`], where every knob is optional and
/// absent means "use the default".
#[optfield(
    pub ConfigFile,
    doc = "The on-disk schema of `config.toml`, generated from [`Config`] \
           with every knob optional — absent = \"use the default\". Field \
           semantics are documented on [`Config`]; [`ConfigFile::resolve`] \
           is where the two meet. A separate type so load → change → save \
           round-trips are honest: only keys the user actually set (plus \
           whatever the change sets) ever exist in the file — defaults stay \
           implicit and follow aurox upgrades instead of being frozen at \
           whatever they were when the file was written.",
    attrs = (derive(Debug, Clone, Default, Deserialize, Serialize)),
    field_attrs = (serde(skip_serializing_if = "Option::is_none")),
    merge_fn = merge_config_file
)]
#[derive(Debug, Clone)]
pub struct Config {
    /// Where per-pkgbase worktrees live.
    pub build_dir: PathBuf,
    /// Enable the AUR half of aurox. `false` is pacman-only mode: no mirror
    /// clone (and no bootstrap prompt), no AUR search/info/install/upgrades —
    /// `-Sy`/`refresh` touch the official-repo databases only. Flip back to
    /// `true` (or delete the line) and run `refresh` to opt into the one-time
    /// ~2 GiB mirror clone.
    pub aur: bool,
    /// Git URL of the AUR mirror to clone.
    pub mirror_url: String,
    /// Abort a fetch if the HTTP transport sees fewer than 1 byte/sec
    /// for this many seconds. Wired into gix's `http.lowSpeedTime` /
    /// `http.lowSpeedLimit` so the curl backend enforces it at the syscall
    /// level. Set to 0 to disable.
    pub mirror_idle_timeout_secs: u64,
    /// Same guard for the one-off bootstrap clone, which needs a far larger
    /// window: GitHub prepares the full ~155k-branch mirror pack server-side
    /// for minutes without sending a byte (protocol V2 without `sideband-all`
    /// carries no progress or keepalives before the packfile section), so the
    /// incremental `mirror_idle_timeout_secs` would misread the wait as a dead
    /// connection and kill every bootstrap. Set to 0 to disable.
    pub bootstrap_idle_timeout_secs: u64,
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
    /// On `-Sy`, also refresh the official-repo databases (rootless, in
    /// parallel with the AUR mirror fetch) so `-Qu`/`-Syu` reflect the latest
    /// pacman-repo versions without a privileged `pacman -Sy`. Set `false` if
    /// you keep the system db current yourself and want `-Sy` to touch the AUR
    /// mirror only.
    pub check_repo_updates: bool,
    /// Legacy knob whose only remaining effect is the [`Self::aur_approval`]
    /// fallback: `"skip"` auto-approves staged AUR packages when
    /// `aur_approval` is unset (pre-`aur_approval` configs keep working).
    /// It no longer controls the `-S` PKGBUILD review prompt — that always
    /// asks, and only `--noconfirm` collapses it.
    pub review_default: String,
    /// `review` | `auto` — whether staged AUR packages need review before
    /// `apply` will run them. `review` (default) puts every AUR item behind the
    /// shell's approval gate; `auto` stages them pre-approved. When unset (the
    /// `None` here), the legacy `review_default == "skip"` behaviour still
    /// auto-approves, so existing configs keep working. Repo packages always
    /// auto-approve regardless. Resolved by
    /// [`AurApproval::from_config`](crate::cli::shell::cart::AurApproval::from_config).
    pub aur_approval: Option<AurApproval>,
    /// Max commits `find_installed_commit` walks back through a pkgbase's
    /// history when looking for the commit that produced the installed
    /// version (so the review screen can diff against it). Fast-moving
    /// pkgs (dotnet-core-*-bin, kernel-git, etc.) can have hundreds of
    /// commits between the user's installed version and HEAD; raise this
    /// when the review screen keeps falling back to "full PKGBUILD" for
    /// those. Cost is ~1ms per commit (clone + parse .SRCINFO), so 256
    /// caps at <300ms even for cold caches.
    pub review_history_scan_max: usize,
}

impl Default for Config {
    fn default() -> Self {
        defaults::default_config()
    }
}

impl Config {
    /// Load the resolved view from `config.toml` if present, else defaults.
    /// Callers that may *change* the config want [`ConfigHandle::load`].
    pub fn load() -> Result<Self> {
        Ok(ConfigHandle::load()?.cfg().clone())
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

impl ConfigFile {
    /// Parse the file at `path` (schema-validated); a missing file is the
    /// empty config (every knob at its default).
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write back to `path`, creating parent directories as needed. Only the
    /// `Some` fields serialize — the file stays as sparse as the user keeps it.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Fill every unset knob with its default — the optfield-generated
    /// `merge_config_file` applies the `Some` fields over
    /// [`defaults::default_config`].
    pub fn resolve(self) -> Config {
        let mut cfg = defaults::default_config();
        cfg.merge_config_file(self);
        cfg
    }
}

/// A loaded configuration bound to its origin: the resolved [`Config`], the
/// sparse [`ConfigFile`] it came from, and the path it round-trips through.
///
/// The one value to thread around when config *changes* are possible: a
/// change goes through [`Self::update`], which edits the file struct, writes
/// it back to the same path it was loaded from, and re-resolves the runtime
/// view — so the three can never disagree, custom config paths included.
#[derive(Debug, Clone)]
pub struct ConfigHandle {
    file: ConfigFile,
    path: PathBuf,
    cfg: Config,
}

impl ConfigHandle {
    /// Load from the default location, [`paths::config_path`].
    pub fn load() -> Result<Self> {
        Self::load_from(paths::config_path())
    }

    /// Load from an explicit path (tests; a future `--config` flag).
    pub fn load_from(path: PathBuf) -> Result<Self> {
        let file = ConfigFile::load(&path)?;
        Ok(Self {
            cfg: file.clone().resolve(),
            file,
            path,
        })
    }

    /// Where this configuration lives on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The resolved runtime view. Read-only: a change goes through
    /// [`Self::update`] so it also lands on disk.
    pub const fn cfg(&self) -> &Config {
        &self.cfg
    }

    /// Change config knobs: apply `change` to the on-disk schema, save it
    /// back to the path this handle was loaded from, and re-resolve
    /// [`Self::cfg`] — one step, no way for the file and the runtime view to
    /// drift apart.
    ///
    /// The one place aurox writes its own (otherwise user-authored) config —
    /// e.g. the first-launch prompt's "pacman-only from now on" answer must
    /// outlive the session, and a visible config line is the transparent way
    /// to remember it.
    pub fn update(&mut self, change: impl FnOnce(&mut ConfigFile)) -> Result<()> {
        change(&mut self.file);
        self.file.save(&self.path)?;
        self.cfg = self.file.clone().resolve();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pacman-only knob must default on (existing configs say nothing
    /// about it) and parse off.
    #[test]
    fn aur_defaults_on_and_parses_off() {
        let cfg = toml::from_str::<ConfigFile>("")
            .expect("empty config parses")
            .resolve();
        assert!(cfg.aur, "missing `aur` key defaults to enabled");
        let cfg = toml::from_str::<ConfigFile>("aur = false")
            .expect("`aur = false` parses")
            .resolve();
        assert!(!cfg.aur);
    }

    /// "Pacman-only from now on" must create a missing config file holding
    /// exactly the one changed knob — no defaults materialize — and flip the
    /// handle's resolved view in the same step.
    #[test]
    fn update_creates_missing_config() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("aurox").join("config.toml");
        let mut config = ConfigHandle::load_from(path.clone()).unwrap();
        assert!(config.cfg().aur);
        config.update(|c| c.aur = Some(false)).unwrap();
        assert!(!config.cfg().aur, "resolved view must follow the update");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.trim(), "aur = false");
        assert!(!ConfigHandle::load_from(path).unwrap().cfg().aur);
    }

    /// …and edit an existing one without touching the user's other keys —
    /// including flipping an explicit `aur = true` in place. (Comments do not
    /// survive the round-trip: the file is re-serialized from the schema.)
    #[test]
    fn update_keeps_user_set_keys_and_stays_sparse() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("config.toml");
        std::fs::write(&path, "aur = true\nindex_threads = 8\n").unwrap();
        let mut config = ConfigHandle::load_from(path.clone()).unwrap();
        config.update(|c| c.aur = Some(false)).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!config.cfg().aur, "aur = true must flip to false:\n{text}");
        assert_eq!(
            config.cfg().index_threads,
            8,
            "user-set key must survive:\n{text}"
        );
        crate::assert_not_contains!(text, "mirror_url");
    }

    /// A config knob the file never set resolves to its default, and a save
    /// round-trip keeps it unset rather than materializing it.
    #[test]
    fn unset_knobs_stay_unset_across_a_round_trip() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("config.toml");
        std::fs::write(&path, "index_threads = 3\n").unwrap();
        let mut config = ConfigHandle::load_from(path.clone()).unwrap();
        config.update(|_| {}).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            "index_threads = 3"
        );
    }
}
