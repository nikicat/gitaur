//! Read-only alpm handle helpers.

use crate::error::Result;
use alpm::Alpm;
use tracing::{debug, instrument};

/// Open the system alpm DB read-only.
pub fn open() -> Result<Alpm> {
    let handle = Alpm::new("/", "/var/lib/pacman")?;
    Ok(handle)
}

/// Installed version of `name`, or `None` if not installed.
#[instrument(skip(alpm))]
pub fn installed_version(alpm: &Alpm, name: &str) -> Option<String> {
    let v = alpm
        .localdb()
        .pkg(name)
        .ok()
        .map(|p| p.version().to_string());
    debug!(name, found = v.is_some(), "localdb lookup");
    v
}

/// True if any syncdb provides `name` (by exact pkgname or virtual provide).
#[instrument(skip(alpm))]
pub fn syncdb_provides(alpm: &Alpm, name: &str) -> bool {
    for db in alpm.syncdbs() {
        if db.pkg(name).is_ok() {
            return true;
        }
        if db
            .pkgs()
            .iter()
            .any(|p| p.provides().iter().any(|d| d.name() == name))
        {
            return true;
        }
    }
    false
}

/// List pkgnames installed locally but not present in any syncdb (foreign pkgs).
pub fn foreign_pkgs(alpm: &Alpm) -> Vec<(String, String)> {
    let sync_names: std::collections::HashSet<&str> = alpm
        .syncdbs()
        .iter()
        .flat_map(|db| db.pkgs().iter().map(|p| p.name()))
        .collect();
    alpm.localdb()
        .pkgs()
        .iter()
        .filter(|p| !sync_names.contains(p.name()))
        .map(|p| (p.name().to_string(), p.version().to_string()))
        .collect()
}
