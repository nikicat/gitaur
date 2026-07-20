//! Classify a single name against pacman + the AUR index — the resolver's one
//! classification site.
//!
//! [`classify_full`] does the lookups once and reports *both* where pacman
//! would get the name and which AUR entry (if any) also claims it. Downstream:
//! [`Source`] (via [`Classification::source`]) is the pacman-precedence plan
//! bucket the resolver walks; the retained AUR entry lets
//! [`crate::resolver::pkgbase_expand`] decide a rewrite without re-running the
//! same `by_name / by_provides / by_pkgbase` ladder. `classify` is the thin
//! `Source`-only wrapper for the dependency walk.

use crate::index::lookup::Lookup;
use crate::index::{EntryIdx, IndexFile};
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
    /// AUR pkgbase — a typed handle into the index (see [`EntryIdx`]).
    Aur(EntryIdx),
    /// Could not be resolved anywhere.
    Missing,
}

/// Where pacman would satisfy a name, if at all — the pacman half of a
/// [`Classification`], carrying the concrete pkgname pacman would act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PacHit {
    /// Present in localdb (by name or an installed `provides`).
    Installed(PkgName),
    /// Present in a sync repo (by name or a sync `provides`).
    Repo(PkgName),
}

/// Which AUR lookup matched a name, plus what each path needs downstream. The
/// path is retained so expansion picks its rewrite without a second lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AurVia {
    /// `name` is a pkgname of the entry (`by_name`).
    Pkgname,
    /// `name` matched a `provides`; carries the resolved provider pkgname.
    Provides(PkgName),
    /// `name` is the entry's pkgbase (`by_pkgbase`).
    Pkgbase,
}

/// An AUR entry that claims a name — its index into `idx.entries` plus the
/// path that matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AurHit {
    pub entry: EntryIdx,
    pub via: AurVia,
}

/// The single classification of a name: where pacman would get it *and* which
/// AUR entry (if any) also claims it — computed in one lookup pass.
///
/// Both are retained deliberately. Plan routing follows pacman precedence
/// ([`Self::source`]); but expansion needs the AUR entry *even when pacman
/// wins* — a foreign-installed split pkgname whose pkgbase ships siblings still
/// needs the `-U` install filter (`decide_pacman_wins`). Reporting both here is
/// what lets the `by_name / by_provides / by_pkgbase` ladder run exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub pac: Option<PacHit>,
    pub aur: Option<AurHit>,
}

impl Classification {
    /// The plan bucket under pacman precedence — pacman wins a shared name, and
    /// the pin / rebuild overrides ([`crate::resolver`]) refine this for direct
    /// targets. Virtuals already resolved to a concrete pkgname in `pac`.
    pub fn source(&self) -> Source {
        match (&self.pac, &self.aur) {
            (Some(PacHit::Installed(n)), _) => Source::Installed(n.clone()),
            (Some(PacHit::Repo(n)), _) => Source::Repo(n.clone()),
            (None, Some(hit)) => Source::Aur(hit.entry),
            (None, None) => Source::Missing,
        }
    }
}

/// Classify `name` (already stripped of any version constraint) in one pass.
///
/// Pacman precedence for *routing* is applied by [`Classification::source`], not
/// here — this reports every claim on the name. Inside the AUR, pkgname beats
/// provides beats pkgbase; the pkgbase fallback lets users type `-S bisq` for an
/// entry whose pkgname is `bisq-desktop`. With no AUR data in play `by` is
/// simply *empty* (see `AurIndexData::load`), so `aur` is `None`.
pub fn classify_full(
    idx: &IndexFile,
    by: &Lookup,
    pac: &PacmanIndex,
    name: &str,
) -> Classification {
    let pac_hit = pac.resolve_concrete(name).map(|(concrete, installed)| {
        if installed {
            PacHit::Installed(concrete.clone())
        } else {
            PacHit::Repo(concrete.clone())
        }
    });
    let aur_hit = if let Some(&i) = by.by_name.get(name) {
        Some(AurHit {
            entry: EntryIdx::new(i as usize),
            via: AurVia::Pkgname,
        })
    } else if let Some((entry, pkgname)) = by.provider_of(idx, name) {
        Some(AurHit {
            entry: EntryIdx::new(entry),
            via: AurVia::Provides(pkgname.clone()),
        })
    } else if let Some(&i) = by.by_pkgbase.get(name) {
        Some(AurHit {
            entry: EntryIdx::new(i as usize),
            via: AurVia::Pkgbase,
        })
    } else {
        None
    };
    Classification {
        pac: pac_hit,
        aur: aur_hit,
    }
}

/// The `Source`-only view for the dependency walk, where a name is genuinely
/// unclassified and no AUR entry needs to survive.
pub fn classify(idx: &IndexFile, by: &Lookup, pac: &PacmanIndex, name: &str) -> Source {
    classify_full(idx, by, pac, name).source()
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

    fn fixture() -> (IndexFile, Lookup, PacmanIndex) {
        let idx = IndexFile {
            entries: vec![
                mk_aur("cower", &["cower"], &[]),
                mk_aur("paru-bin", &["paru-bin"], &["paru"]),
                // Same name in pacman: pacman wins despite this AUR entry.
                mk_aur("firefox-nightly", &["firefox-nightly"], &["firefox"]),
            ],
            ..IndexFile::empty()
        };
        let by = Lookup::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.installed.insert("vim".into(), "9.0-1".into());
        pac.sync_versions.insert("firefox".into(), "110.0-1".into());
        pac.sync_providers
            .insert("java-runtime".into(), vec!["jre-openjdk".into()]);
        (idx, by, pac)
    }

    /// The "no AUR data" view: an empty lookup, exactly what
    /// `AurIndexData::empty()` feeds the resolver.
    fn empty_by() -> Lookup {
        Lookup::build(&IndexFile::empty())
    }

    #[test]
    fn installed_wins_everything() {
        let (idx, by, pac) = fixture();
        assert_eq!(
            classify(&idx, &by, &pac, "vim"),
            Source::Installed("vim".into())
        );
    }

    #[test]
    fn pacman_wins_over_aur() {
        // `firefox` is both in the sync repos *and* provided by `firefox-nightly`
        // in the AUR fixture — pacman must take precedence.
        let (idx, by, pac) = fixture();
        assert_eq!(
            classify(&idx, &by, &pac, "firefox"),
            Source::Repo("firefox".into())
        );
    }

    #[test]
    fn virtual_provide_resolves_to_concrete_sync_provider() {
        // `java-runtime` is a virtual `provides`; classify must rewrite to
        // the providing pkgname (`jre-openjdk`) so the plan never shows a
        // fake "package".
        let (idx, by, pac) = fixture();
        assert_eq!(
            classify(&idx, &by, &pac, "java-runtime"),
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
            classify(&IndexFile::empty(), &empty_by(), &pac, "cargo"),
            Source::Installed("rust".into())
        );
    }

    #[test]
    fn aur_when_pacman_misses() {
        let (idx, by, pac) = fixture();
        assert!(matches!(classify(&idx, &by, &pac, "cower"), Source::Aur(_)));
    }

    #[test]
    fn aur_provides_fallback() {
        let (idx, by, pac) = fixture();
        assert!(matches!(classify(&idx, &by, &pac, "paru"), Source::Aur(_)));
    }

    #[test]
    fn missing_without_aur_index() {
        // No AUR data (pure pacman environment): the empty lookup yields
        // Missing for AUR-only names…
        let (idx, _by, pac) = fixture();
        let by = empty_by();
        assert_eq!(classify(&idx, &by, &pac, "cower"), Source::Missing);
        // …but pacman-resolvable names still classify correctly.
        assert_eq!(
            classify(&idx, &by, &pac, "firefox"),
            Source::Repo("firefox".into())
        );
        assert_eq!(
            classify(&idx, &by, &pac, "vim"),
            Source::Installed("vim".into())
        );
    }

    #[test]
    fn unknown_is_missing() {
        let (idx, by, pac) = fixture();
        assert_eq!(classify(&idx, &by, &pac, "nonexistent"), Source::Missing);
    }
}
