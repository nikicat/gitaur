//! Built-in defaults applied when `config.toml` is absent or a field is missing.

use super::Config;
use crate::paths;

/// Construct a freshly-defaulted [`Config`].
pub fn default_config() -> Config {
    Config {
        build_dir: paths::state_dir().join("pkgs"),
        aur: true,
        mirror_url: "https://github.com/archlinux/aur.git".into(),
        mirror_idle_timeout_secs: 30,
        // 10 minutes: GitHub's silent pack preparation for the full mirror
        // took >30s but well under a minute when measured (2026-07); the wide
        // margin covers slower mirrors without leaving a truly dead bootstrap
        // hanging forever.
        bootstrap_idle_timeout_secs: 600,
        index_threads: 4,
        refresh_max_age_secs: 3600,
        color: "auto".into(),
        makepkg_path: "makepkg".into(),
        // `-d` skips makepkg's own dep checks: aurox pre-installs makedeps
        // stratum-by-stratum, and `makepkg -s` would otherwise try to fetch
        // AUR-only deps via `pacman -S` and fail. Runtime `depends` are
        // satisfied later by the final `pacman -U` resolving intra-stratum.
        makepkg_args: vec!["-d".into(), "--noconfirm".into(), "--needed".into()],
        privilege_escalator: "sudo".into(),
        devel: false,
        check_repo_updates: true,
        review_default: "prompt".into(),
        // Unset: `aur_policy` defers to `review_default` for back-compat. Set to
        // `Some(AurApproval::{Review,Auto})` in config.toml to pin the gate.
        aur_approval: None,
        // 256 covers ~2 years of dotnet-core-7.0-bin-shaped pkgs (~10
        // updates/month). Old default of 64 routinely missed the diff
        // base on long-untouched installs; bumping the headline cost
        // (~250ms one-shot) is worth always finding the commit when it
        // exists in the AUR repo.
        review_history_scan_max: 256,
    }
}
