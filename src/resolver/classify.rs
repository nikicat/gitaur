//! Classify a single dep reference into Installed / Repo / AUR / Missing.

use crate::index::secondary::Secondary;
use crate::pacman::alpm_db::PacmanIndex;

/// Where a given dep name lives.
///
/// Resolution order (pacman wins when both have the pkg):
///   1. local pacman DB                   → Installed
///   2. sync pacman repos                 → Repo
///   3. AUR index by pkgname              → Aur(idx)
///   4. AUR index by `provides`           → Aur(idx)
///   5. AUR index by pkgbase (yay-style)  → Aur(idx)
///   6. neither                           → Missing
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Already in the local pacman DB; nothing to do.
    Installed,
    /// Available in a sync repo; install via pacman batch.
    Repo,
    /// AUR pkgbase at `idx.entries[usize]`.
    Aur(usize),
    /// Could not be resolved anywhere.
    Missing,
}

/// Classify `name` (already stripped of any version constraint).
///
/// Pacman precedence: a name resolvable from pacman is never routed through
/// AUR even if AUR has its own copy — matches yay/paru convention. Inside the
/// AUR, pkgname beats provides beats pkgbase; the pkgbase fallback lets users
/// type `-S bisq` for an entry whose pkgname is `bisq-desktop`.
pub fn classify(by: Option<&Secondary>, pac: &PacmanIndex, name: &str) -> Source {
    if pac.is_installed(name) {
        return Source::Installed;
    }
    if pac.in_sync(name) {
        return Source::Repo;
    }
    let Some(by) = by else {
        return Source::Missing;
    };
    if let Some(&i) = by.by_name.get(name) {
        return Source::Aur(i as usize);
    }
    if let Some(providers) = by.by_provides.get(name) {
        if let Some(&i) = providers.first() {
            return Source::Aur(i as usize);
        }
    }
    if let Some(&i) = by.by_pkgbase.get(name) {
        return Source::Aur(i as usize);
    }
    Source::Missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::schema::{IndexEntry, IndexFile, Pkgname};

    fn mk_aur(pkgbase: &str, names: &[&str], provides: &[&str]) -> IndexEntry {
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: names
                .iter()
                .map(|s| Pkgname {
                    name: (*s).into(),
                    provides: Vec::new(),
                })
                .collect(),
            provides: provides.iter().map(|s| (*s).into()).collect(),
            pkgver: "1".into(),
            pkgrel: "1".into(),
            ..Default::default()
        }
    }

    fn fixture() -> (IndexFile, Secondary, PacmanIndex) {
        let idx = IndexFile {
            entries: vec![
                mk_aur("cower", &["cower"], &[]),
                mk_aur("paru-bin", &["paru-bin"], &["paru"]),
                // Same name in pacman: pacman wins despite this AUR entry.
                mk_aur("firefox-nightly", &["firefox-nightly"], &["firefox"]),
            ],
            ..IndexFile::empty()
        };
        let by = Secondary::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.installed.insert("vim".into(), "9.0-1".into());
        pac.sync_names.insert("firefox".into());
        pac.sync_provides.insert("java-runtime".into());
        (idx, by, pac)
    }

    #[test]
    fn installed_wins_everything() {
        let (_idx, by, pac) = fixture();
        assert_eq!(classify(Some(&by), &pac, "vim"), Source::Installed);
    }

    #[test]
    fn pacman_wins_over_aur() {
        // `firefox` is both in the sync repos *and* provided by `firefox-nightly`
        // in the AUR fixture — pacman must take precedence.
        let (_idx, by, pac) = fixture();
        assert_eq!(classify(Some(&by), &pac, "firefox"), Source::Repo);
    }

    #[test]
    fn aur_when_pacman_misses() {
        let (_idx, by, pac) = fixture();
        assert!(matches!(classify(Some(&by), &pac, "cower"), Source::Aur(_)));
    }

    #[test]
    fn aur_provides_fallback() {
        let (_idx, by, pac) = fixture();
        assert!(matches!(classify(Some(&by), &pac, "paru"), Source::Aur(_)));
    }

    #[test]
    fn missing_without_aur_index() {
        // No AUR index loaded (pure pacman environment).
        let (_idx, _by, pac) = fixture();
        assert_eq!(classify(None, &pac, "cower"), Source::Missing);
        // …but pacman-resolvable names still classify correctly.
        assert_eq!(classify(None, &pac, "firefox"), Source::Repo);
        assert_eq!(classify(None, &pac, "vim"), Source::Installed);
    }

    #[test]
    fn unknown_is_missing() {
        let (_idx, by, pac) = fixture();
        assert_eq!(classify(Some(&by), &pac, "nonexistent"), Source::Missing);
    }
}
