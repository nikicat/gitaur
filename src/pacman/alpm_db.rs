//! Read-only alpm handle helpers + a precomputed `PacmanIndex` snapshot.
//!
//! `alpm::Alpm` is `Send` but not `Sync`, so we can't share it across rayon
//! workers. `PacmanIndex` reads everything we need from `&Alpm` once into
//! owned hash structures, making subsequent lookups pure data — Sync, cheap,
//! and parallelisable.

use crate::error::{Error, Result};
use alpm::Alpm;
use std::collections::HashMap;
use tracing::{debug, instrument};

/// Open the system alpm DB with sync repos registered from `pacman.conf`.
///
/// `Alpm::new` alone gives an empty `syncdbs()` — sync repos are pacman.conf
/// state, not alpm state. We parse the config and let `alpm-utils` register
/// every `[repo]` section.
pub fn open() -> Result<Alpm> {
    let conf =
        pacmanconf::Config::new().map_err(|e| Error::other(format!("read pacman.conf: {e}")))?;
    alpm_utils::alpm_with_conf(&conf).map_err(|e| Error::other(format!("open alpm with conf: {e}")))
}

/// Snapshot of the local + sync pacman DBs as immutable hash structures.
///
/// Built once at the top of `cmd_install` so per-target classification is
/// a sequence of `HashMap` / `HashSet` lookups — Sync (no `&Alpm` to share),
/// O(1) per query, and safe to call from rayon workers.
#[derive(Debug, Default)]
pub struct PacmanIndex {
    /// pkgname → installed version (from localdb).
    pub installed: HashMap<String, String>,
    /// virtual provide name → installed pkgnames declaring it. Used to mark
    /// a dependency as already-satisfied — if any provider is installed,
    /// `pacman -S --needed` would no-op, so the plan must drop it instead
    /// of pretending to install a virtual.
    pub installed_providers: HashMap<String, Vec<String>>,
    /// pkgname → version available in some sync repo. Repo precedence is
    /// pacman's: the first DB declared in `pacman.conf` wins on duplicates.
    pub sync_versions: HashMap<String, String>,
    /// virtual provide name → sync-repo pkgnames declaring it. When a
    /// dependency is a virtual name we pick a concrete provider so the plan
    /// shows the package pacman would actually install, with its version.
    pub sync_providers: HashMap<String, Vec<String>>,
}

impl PacmanIndex {
    /// Snapshot `&Alpm` into owned hash tables. Single pass over each DB.
    #[instrument(skip(alpm))]
    pub fn build(alpm: &Alpm) -> Self {
        let mut installed: HashMap<String, String> = HashMap::new();
        let mut installed_providers: HashMap<String, Vec<String>> = HashMap::new();
        for p in alpm.localdb().pkgs() {
            let name = p.name().to_string();
            installed.insert(name.clone(), p.version().to_string());
            for prov in p.provides() {
                installed_providers
                    .entry(prov.name().to_string())
                    .or_default()
                    .push(name.clone());
            }
        }
        let mut sync_versions: HashMap<String, String> = HashMap::new();
        let mut sync_providers: HashMap<String, Vec<String>> = HashMap::new();
        for db in alpm.syncdbs() {
            for p in db.pkgs() {
                let name = p.name().to_string();
                // `entry().or_insert` so the first DB pacman.conf lists wins,
                // matching pacman's own repo precedence.
                sync_versions
                    .entry(name.clone())
                    .or_insert_with(|| p.version().to_string());
                for prov in p.provides() {
                    sync_providers
                        .entry(prov.name().to_string())
                        .or_default()
                        .push(name.clone());
                }
            }
        }
        debug!(
            installed = installed.len(),
            installed_provides = installed_providers.len(),
            sync = sync_versions.len(),
            sync_provides = sync_providers.len(),
            "pacman index built"
        );
        Self {
            installed,
            installed_providers,
            sync_versions,
            sync_providers,
        }
    }

    /// Installed version of `name`, or `None` if not installed.
    pub fn installed_version(&self, name: &str) -> Option<&str> {
        self.installed.get(name).map(String::as_str)
    }

    /// Already installed locally?
    pub fn is_installed(&self, name: &str) -> bool {
        self.installed.contains_key(name)
    }

    /// Available in a sync repo, either by exact name or by virtual provide?
    pub fn in_sync(&self, name: &str) -> bool {
        self.sync_versions.contains_key(name) || self.sync_providers.contains_key(name)
    }

    /// Sync-repo version for `name`, or `None` when `name` is not an exact
    /// pkgname in any syncdb. Matches by-name only — virtual `provides` aren't
    /// versioned (their version, if any, lives on the providing pkg) so a
    /// provides hit deliberately returns `None`.
    pub fn sync_version(&self, name: &str) -> Option<&str> {
        self.sync_versions.get(name).map(String::as_str)
    }

    /// Resolve a (possibly virtual) name to the concrete pkgname pacman would
    /// act on, paired with whether it's already installed.
    ///
    /// Order:
    ///   1. exact installed pkgname → `(name, true)`
    ///   2. an installed pkg providing the virtual → `(provider, true)`
    ///   3. exact sync pkgname → `(name, false)`
    ///   4. a sync pkg providing the virtual → `(first_provider, false)`
    ///   5. nothing pacman knows about → `None`
    ///
    /// "Installed providers win" is the load-bearing choice: `pacman -S --needed`
    /// on an already-satisfied virtual is a no-op, so the plan must drop the
    /// dep instead of staging a redundant install of a different concrete pkg.
    /// On a sync-providers tie we pick the first one we saw (DB declaration
    /// order from `pacman.conf`); pacman would prompt, we don't.
    pub fn resolve_concrete(&self, name: &str) -> Option<(&str, bool)> {
        if let Some((n, _)) = self.installed.get_key_value(name) {
            return Some((n.as_str(), true));
        }
        if let Some(provs) = self.installed_providers.get(name) {
            if let Some(p) = provs.first() {
                return Some((p.as_str(), true));
            }
        }
        if let Some((n, _)) = self.sync_versions.get_key_value(name) {
            return Some((n.as_str(), false));
        }
        if let Some(provs) = self.sync_providers.get(name) {
            if let Some(p) = provs.first() {
                return Some((p.as_str(), false));
            }
        }
        None
    }

    /// pkgnames installed locally but not present in any syncdb (foreign).
    pub fn foreign(&self) -> Vec<(String, String)> {
        self.installed
            .iter()
            .filter(|(name, _)| !self.sync_versions.contains_key(name.as_str()))
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookups_use_owned_hashes() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("vim".into(), "9.0-1".into());
        idx.sync_versions.insert("firefox".into(), "110.0-1".into());
        idx.sync_providers
            .insert("java-runtime".into(), vec!["jre-openjdk".into()]);

        assert!(idx.is_installed("vim"));
        assert!(!idx.is_installed("firefox"));
        assert!(idx.in_sync("firefox"));
        assert!(idx.in_sync("java-runtime"));
        assert!(!idx.in_sync("nonexistent"));
        assert_eq!(idx.installed_version("vim"), Some("9.0-1"));
        assert_eq!(idx.installed_version("firefox"), None);
        assert_eq!(idx.sync_version("firefox"), Some("110.0-1"));
        // Provides-only names carry no version of their own — only the
        // providing pkgname does.
        assert_eq!(idx.sync_version("java-runtime"), None);
    }

    #[test]
    fn foreign_excludes_sync_pkgs() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("vim".into(), "9.0-1".into());
        idx.installed.insert("paru-bin".into(), "2.0.0-1".into());
        idx.sync_versions.insert("vim".into(), "9.0-1".into());

        let mut foreign = idx.foreign();
        foreign.sort();
        assert_eq!(foreign, vec![("paru-bin".into(), "2.0.0-1".into())]);
    }

    /// `resolve_concrete` is the single source of truth for "what would
    /// pacman actually install if I asked for this name?". Cover every
    /// branch: exact installed, installed-via-provides, exact sync, sync-
    /// via-provides, and unknown.
    #[test]
    fn resolve_concrete_orders_installed_before_sync() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("rust".into(), "1.80-1".into());
        idx.installed_providers
            .insert("cargo".into(), vec!["rust".into()]);
        idx.sync_versions.insert("pacman".into(), "6.1.0-1".into());
        idx.sync_providers
            .insert("libalpm.so".into(), vec!["pacman".into()]);
        idx.sync_versions.insert("rustup".into(), "1.27-1".into());
        // rustup also provides cargo, but rust (installed) must win.
        idx.sync_providers
            .entry("cargo".into())
            .or_default()
            .push("rustup".into());

        assert_eq!(idx.resolve_concrete("rust"), Some(("rust", true)));
        assert_eq!(idx.resolve_concrete("cargo"), Some(("rust", true)));
        assert_eq!(idx.resolve_concrete("pacman"), Some(("pacman", false)));
        assert_eq!(idx.resolve_concrete("libalpm.so"), Some(("pacman", false)));
        assert_eq!(idx.resolve_concrete("nonexistent"), None);
    }
}
