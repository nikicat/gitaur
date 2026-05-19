//! Read-only alpm handle helpers + a precomputed `PacmanIndex` snapshot.
//!
//! `alpm::Alpm` is `Send` but not `Sync`, so we can't share it across rayon
//! workers. `PacmanIndex` reads everything we need from `&Alpm` once into
//! owned hash structures, making subsequent lookups pure data — Sync, cheap,
//! and parallelisable.

use crate::error::{Error, Result};
use alpm::Alpm;
use std::collections::{HashMap, HashSet};
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
    /// pkgname → pkgbase recorded in localdb metadata, when present.
    /// Lets us tie split subpackages (notably `*-debug`) back to their
    /// pkgbase so they aren't surfaced as unknown foreign pkgs.
    pub installed_base: HashMap<String, String>,
    /// Set of exact pkgnames available across all sync repos.
    pub sync_names: HashSet<String>,
    /// Set of virtual `provides` names exposed by any sync-repo pkg.
    pub sync_provides: HashSet<String>,
}

impl PacmanIndex {
    /// Snapshot `&Alpm` into owned hash tables. Single pass over each DB.
    #[instrument(skip(alpm))]
    pub fn build(alpm: &Alpm) -> Self {
        let mut installed: HashMap<String, String> = HashMap::new();
        let mut installed_base: HashMap<String, String> = HashMap::new();
        for p in alpm.localdb().pkgs() {
            let name = p.name().to_string();
            installed.insert(name.clone(), p.version().to_string());
            if let Some(base) = p.base() {
                installed_base.insert(name, base.to_string());
            }
        }
        let mut sync_names: HashSet<String> = HashSet::new();
        let mut sync_provides: HashSet<String> = HashSet::new();
        for db in alpm.syncdbs() {
            for p in db.pkgs() {
                sync_names.insert(p.name().to_string());
                for prov in p.provides() {
                    sync_provides.insert(prov.name().to_string());
                }
            }
        }
        debug!(
            installed = installed.len(),
            sync = sync_names.len(),
            provides = sync_provides.len(),
            "pacman index built"
        );
        Self {
            installed,
            installed_base,
            sync_names,
            sync_provides,
        }
    }

    /// pkgbase recorded for `name` in the local DB, if any.
    pub fn installed_pkgbase(&self, name: &str) -> Option<&str> {
        self.installed_base.get(name).map(String::as_str)
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
        self.sync_names.contains(name) || self.sync_provides.contains(name)
    }

    /// pkgnames installed locally but not present in any syncdb (foreign).
    pub fn foreign(&self) -> Vec<(String, String)> {
        self.installed
            .iter()
            .filter(|(name, _)| !self.sync_names.contains(name.as_str()))
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
        idx.sync_names.insert("firefox".into());
        idx.sync_provides.insert("java-runtime".into());

        assert!(idx.is_installed("vim"));
        assert!(!idx.is_installed("firefox"));
        assert!(idx.in_sync("firefox"));
        assert!(idx.in_sync("java-runtime"));
        assert!(!idx.in_sync("nonexistent"));
        assert_eq!(idx.installed_version("vim"), Some("9.0-1"));
        assert_eq!(idx.installed_version("firefox"), None);
    }

    #[test]
    fn installed_pkgbase_round_trips() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("foo-debug".into(), "1-1".into());
        idx.installed_base.insert("foo-debug".into(), "foo".into());
        assert_eq!(idx.installed_pkgbase("foo-debug"), Some("foo"));
        assert_eq!(idx.installed_pkgbase("foo"), None);
    }

    #[test]
    fn foreign_excludes_sync_pkgs() {
        let mut idx = PacmanIndex::default();
        idx.installed.insert("vim".into(), "9.0-1".into());
        idx.installed.insert("paru-bin".into(), "2.0.0-1".into());
        idx.sync_names.insert("vim".into());

        let mut foreign = idx.foreign();
        foreign.sort();
        assert_eq!(foreign, vec![("paru-bin".into(), "2.0.0-1".into())]);
    }
}
