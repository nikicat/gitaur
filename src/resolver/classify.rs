//! Classify a single dep reference into Installed / Repo / AUR / Missing.

use crate::index::secondary::Secondary;
use crate::names::PkgName;
use crate::pacman::alpm_db::PacmanIndex;

/// Where a given dep name lives, with the **concrete** pkgname pacman would act on.
///
/// Virtual `provides` (`cargo`, `libalpm.so`, …) get resolved to their
/// provider here so the plan never displays a fake "package".
///
/// Resolution order (pacman wins when both have the pkg):
///   1. local pacman DB (by name or installed `provides`)  → Installed
///   2. sync pacman repos (by name or sync `provides`)     → Repo
///   3. AUR index by pkgname                               → Aur(idx)
///   4. AUR index by `provides`                            → Aur(idx)
///   5. AUR index by pkgbase (yay-style)                   → Aur(idx)
///   6. neither                                            → Missing
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Already satisfied locally; nothing to do. Carries the concrete pkgname
    /// (the resolved provider, not the original virtual) for diagnostics.
    Installed(PkgName),
    /// Available in a sync repo; install via pacman batch. Carries the
    /// concrete pkgname pacman will actually install — when the input was a
    /// virtual provide we substitute the provider's pkgname, which is what
    /// shows up in the plan and the eventual `pacman -S` argv.
    Repo(PkgName),
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
/// type `-S bisq` for an entry whose pkgname is `bisq-desktop`. With no AUR
/// data in play `by` is simply *empty* (see `UpgradeSession::load`) and every
/// non-pacman name lands on [`Source::Missing`].
pub fn classify(by: &Secondary, pac: &PacmanIndex, name: &str) -> Source {
    if let Some((concrete, installed)) = pac.resolve_concrete(name) {
        return if installed {
            Source::Installed(concrete.clone())
        } else {
            Source::Repo(concrete.clone())
        };
    }
    if let Some(&i) = by.by_name.get(name) {
        return Source::Aur(i as usize);
    }
    if let Some(providers) = by.by_provides.get(name)
        && let Some(&i) = providers.first()
    {
        return Source::Aur(i as usize);
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
                    pkgdesc: None,
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
        pac.sync_versions.insert("firefox".into(), "110.0-1".into());
        pac.sync_providers
            .insert("java-runtime".into(), vec!["jre-openjdk".into()]);
        (idx, by, pac)
    }

    /// The "no AUR data" view: an empty secondary, exactly what
    /// `UpgradeSession::empty()` feeds the resolver.
    fn empty_by() -> Secondary {
        Secondary::build(&IndexFile::empty())
    }

    #[test]
    fn installed_wins_everything() {
        let (_idx, by, pac) = fixture();
        assert_eq!(classify(&by, &pac, "vim"), Source::Installed("vim".into()));
    }

    #[test]
    fn pacman_wins_over_aur() {
        // `firefox` is both in the sync repos *and* provided by `firefox-nightly`
        // in the AUR fixture — pacman must take precedence.
        let (_idx, by, pac) = fixture();
        assert_eq!(
            classify(&by, &pac, "firefox"),
            Source::Repo("firefox".into())
        );
    }

    #[test]
    fn virtual_provide_resolves_to_concrete_sync_provider() {
        // `java-runtime` is a virtual `provides`; classify must rewrite to
        // the providing pkgname (`jre-openjdk`) so the plan never shows a
        // fake "package".
        let (_idx, by, pac) = fixture();
        assert_eq!(
            classify(&by, &pac, "java-runtime"),
            Source::Repo("jre-openjdk".into())
        );
    }

    #[test]
    fn installed_provider_short_circuits_to_installed() {
        // Locally-installed pkg `rust` provides `cargo`; asking for `cargo`
        // must classify as Installed (with `rust` as the concrete provider),
        // not Repo — so the resolver drops it from the plan.
        let mut pac = PacmanIndex::default();
        pac.installed.insert("rust".into(), "1.80-1".into());
        pac.installed_providers
            .insert("cargo".into(), vec!["rust".into()]);
        pac.sync_providers
            .insert("cargo".into(), vec!["rustup".into()]);
        pac.sync_versions.insert("rustup".into(), "1.27-1".into());
        assert_eq!(
            classify(&empty_by(), &pac, "cargo"),
            Source::Installed("rust".into())
        );
    }

    #[test]
    fn aur_when_pacman_misses() {
        let (_idx, by, pac) = fixture();
        assert!(matches!(classify(&by, &pac, "cower"), Source::Aur(_)));
    }

    #[test]
    fn aur_provides_fallback() {
        let (_idx, by, pac) = fixture();
        assert!(matches!(classify(&by, &pac, "paru"), Source::Aur(_)));
    }

    #[test]
    fn missing_without_aur_index() {
        // No AUR data (pure pacman environment): the empty secondary yields
        // Missing for AUR-only names…
        let (_idx, _by, pac) = fixture();
        let by = empty_by();
        assert_eq!(classify(&by, &pac, "cower"), Source::Missing);
        // …but pacman-resolvable names still classify correctly.
        assert_eq!(
            classify(&by, &pac, "firefox"),
            Source::Repo("firefox".into())
        );
        assert_eq!(classify(&by, &pac, "vim"), Source::Installed("vim".into()));
    }

    #[test]
    fn unknown_is_missing() {
        let (_idx, by, pac) = fixture();
        assert_eq!(classify(&by, &pac, "nonexistent"), Source::Missing);
    }
}
