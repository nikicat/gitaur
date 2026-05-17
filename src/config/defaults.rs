//! Built-in defaults applied when `config.toml` is absent or a field is missing.

use super::Config;
use crate::paths;

/// Construct a freshly-defaulted [`Config`].
pub fn default_config() -> Config {
    Config {
        build_dir: paths::state_dir().join("pkgs"),
        mirror_url: "https://github.com/archlinux/aur.git".into(),
        index_threads: 4,
        refresh_max_age_secs: 3600,
        color: "auto".into(),
        makepkg_path: "makepkg".into(),
        // `-d` skips makepkg's own dep checks: gitaur pre-installs makedeps
        // stratum-by-stratum, and `makepkg -s` would otherwise try to fetch
        // AUR-only deps via `pacman -S` and fail. Runtime `depends` are
        // satisfied later by the final `pacman -U` resolving intra-stratum.
        makepkg_args: vec!["-d".into(), "--noconfirm".into(), "--needed".into()],
        privilege_escalator: "sudo".into(),
        devel: false,
        review_default: "prompt".into(),
    }
}
