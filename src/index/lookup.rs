//! Lookup tables built after loading the primary index.

use crate::index::schema::{IndexEntry, IndexFile};
use crate::names::{PkgBase, PkgName, VirtualName};
use rayon::prelude::*;
use smallvec::SmallVec;
use std::collections::HashMap;
use tracing::{debug, instrument};

/// How AUR classifies a name from pacman's localdb domain.
///
/// Pacman and AUR are two distinct identity domains that share string
/// shape: a single string like `dotnet-runtime-7.0` can be a pacman
/// `PkgName` (registered install), an AUR `VirtualName` (declared in
/// some pkg's `provides=`), an AUR primary `PkgName` (a `pkgname=` line),
/// or an AUR `PkgBase`. The cross-domain question "given this pacman
/// pkgname, what does AUR call it?" is what `classify_foreign` answers,
/// and the tagged enum names which identity matched.
///
/// `'a` borrows from the [`IndexFile`] passed to `classify_foreign`.
#[derive(Debug, Clone, Copy)]
pub enum AurClass<'a> {
    /// Pacman pkgname matches an AUR pkg's primary pkgname (`pkgname=`
    /// in some pkgbase's .SRCINFO). Same identity, both domains.
    AsPkgname(&'a IndexEntry),
    /// Pacman pkgname matches some AUR pkg's virtual `provides=` line.
    /// Cross-domain bridge: pacman-side `PkgName` is lexically equal to
    /// AUR-side `VirtualName`. The dotnet-rename case.
    AsProvides(&'a IndexEntry),
    /// Pacman pkgname matches an AUR pkgbase string. Rare — pacman never
    /// records a pkgbase as a pkg unless someone built and installed an
    /// AUR pkg whose pkgname happens to equal its pkgbase, which is the
    /// canonical non-split case (and then `AsPkgname` would match first).
    AsPkgbase(&'a IndexEntry),
    /// AUR doesn't know about this name in any of the three identities.
    NotInAur,
}

/// Post-load lookup structure: pkgname / provides / pkgbase → index position.
///
/// Each map is keyed on the typed identity it serves: [`PkgName`] for
/// primary pkgnames, [`VirtualName`] for `provides=` declarations,
/// [`PkgBase`] for mirror branches. The three are deliberately distinct
/// at the type level — see [`AurClass`] for the cross-domain bridge.
pub struct Lookup {
    /// pkgname → entries[idx]. Split pkgs map multiple names to the same idx.
    pub by_name: HashMap<PkgName, u32>,
    /// `provides=` virtual name → set of entry indices. Keys are
    /// [`VirtualName`] (NOT [`PkgName`]) — they have distinct semantic
    /// origin (a satisfies-claim, not a registered name).
    pub by_provides: HashMap<VirtualName, SmallVec<[u32; 2]>>,
    /// pkgbase → entries[idx]. Used as a last-resort lookup so users can write
    /// `-S <pkgbase>` (yay-style) even when no pkgname equals the pkgbase —
    /// e.g. pkgbase `bisq` produces pkgname `bisq-desktop`. Resolution order
    /// elsewhere keeps pkgname / provides preferred over pkgbase.
    pub by_pkgbase: HashMap<PkgBase, u32>,
}

impl Lookup {
    /// Build `by_name`, `by_provides`, and `by_pkgbase` over the loaded index.
    #[instrument(skip(idx), fields(entries = idx.entries.len()))]
    pub fn build(idx: &IndexFile) -> Self {
        let mut by_name = HashMap::with_capacity(idx.entries.len() * 2);
        let mut by_provides: HashMap<VirtualName, SmallVec<[u32; 2]>> = HashMap::new();
        let mut by_pkgbase = HashMap::with_capacity(idx.entries.len());
        for (i, e) in idx.entries.iter().enumerate() {
            let i = u32::try_from(i).expect("AUR index entries exceed u32::MAX");
            for pkg in &e.pkgnames {
                by_name.insert(pkg.name.clone(), i);
            }
            // Both pkgbase-level (e.provides) and pkgname-scoped
            // (pkgnames[*].provides) entries index back to the same
            // pkgbase row — by_provides only carries entry indices.
            // Attribution to a specific pkgname lives in `provider_of`,
            // which the resolver uses when rewriting `-S <virtual-name>`
            // into the concrete pkgname.
            for prov in e.all_provides() {
                // Promote the typed dep-spec to a typed `VirtualName` at this
                // single boundary, stripping any version constraint. Distinct
                // from `PkgName` even when lexically identical (the dotnet
                // case).
                by_provides
                    .entry(VirtualName::new(prov.bare()))
                    .or_default()
                    .push(i);
            }
            by_pkgbase.insert(e.pkgbase.clone(), i);
        }
        debug!(
            by_name = by_name.len(),
            by_provides = by_provides.len(),
            by_pkgbase = by_pkgbase.len(),
            "lookup maps built"
        );
        Self {
            by_name,
            by_provides,
            by_pkgbase,
        }
    }

    /// Identify which pkgname inside the entry actually declares `name` as a
    /// `provides`. Useful for rewriting `-S <virtual-name>` into the
    /// concrete pkgname the user really wants (the AUR-RPC behaviour yay /
    /// paru rely on).
    ///
    /// Returns the entry index plus the matching pkgname:
    ///   * pkgname-scoped provides → that pkgname's name.
    ///   * pkgbase-level provides  → the first pkgname in the entry. A
    ///     pkgbase-level `provides` semantically applies to *every* pkgname,
    ///     so picking the first is arbitrary but stable, and matches AUR's
    ///     "build the whole pkgbase" intent when no single pkgname owns the
    ///     virtual name.
    ///
    /// `None` means no provides match anywhere in the index (the resolver
    /// should fall back to pkgbase / Missing).
    pub fn provider_of<'a>(&self, idx: &'a IndexFile, name: &str) -> Option<(usize, &'a PkgName)> {
        let bare = strip_version_constraint(name);
        let &entry_idx = self.by_provides.get(bare)?.first()?;
        let entry = idx.entries.get(entry_idx as usize)?;
        // Prefer the pkgname that explicitly declared this provides; that's
        // the case the bisq/yay parity work was added for.
        for pkg in &entry.pkgnames {
            if pkg.provides.iter().any(|p| p.bare() == bare) {
                return Some((entry_idx as usize, &pkg.name));
            }
        }
        // No pkgname owned it, so the match came from a pkgbase-level
        // provides — every pkgname provides it implicitly. Pick the first
        // for a deterministic answer.
        entry
            .pkgnames
            .first()
            .map(|p| (entry_idx as usize, &p.name))
    }

    /// Classify a pacman-domain pkgname against the AUR index. The
    /// cross-domain bridge: pacman has `name` registered as an installed
    /// pkg; this function asks "what does AUR call this string?" and
    /// returns a tagged enum naming which identity matched. See
    /// [`AurClass`] for the four cases.
    ///
    /// The `HashMap` probes use `Borrow<str>` to compare the underlying
    /// string across the three identity-distinct maps — this is the
    /// *one* place that cross-identity claim is made, and it's named
    /// (`AsProvides`, `AsPkgbase`) in the return value.
    pub fn classify_foreign<'a>(&self, idx: &'a IndexFile, name: &PkgName) -> AurClass<'a> {
        if let Some(i) = self.by_name.get(name) {
            return AurClass::AsPkgname(&idx.entries[*i as usize]);
        }
        // Use `Borrow<str>` to probe maps keyed on the other two identities.
        // `<PkgName as Borrow<str>>::borrow(name)` returns the underlying
        // string slice the typed wrappers share — the cross-identity
        // string-match claim is encapsulated here.
        let s = <PkgName as std::borrow::Borrow<str>>::borrow(name);
        if let Some(providers) = self.by_provides.get(s)
            && let Some(i) = providers.first()
        {
            return AurClass::AsProvides(&idx.entries[*i as usize]);
        }
        if let Some(i) = self.by_pkgbase.get(s) {
            return AurClass::AsPkgbase(&idx.entries[*i as usize]);
        }
        AurClass::NotInAur
    }

    /// Look up an AUR entry by typed [`PkgName`] — `by_name` only, no
    /// virtual / pkgbase fallback. For when the caller already knows the
    /// name should be a primary pkgname in AUR's domain (e.g. resolver
    /// dep lookup). Cross-domain queries against a foreign pkg go
    /// through [`Self::classify_foreign`] instead.
    pub fn lookup_pkgname<'a>(&self, idx: &'a IndexFile, name: &PkgName) -> Option<&'a IndexEntry> {
        let i = self.by_name.get(name)?;
        idx.entries.get(*i as usize)
    }

    /// Look up an AUR entry by typed [`PkgBase`] — `by_pkgbase` only.
    /// Used by the build pipeline once a pkgbase identity is established.
    pub fn lookup_pkgbase<'a>(
        &self,
        idx: &'a IndexFile,
        pkgbase: &PkgBase,
    ) -> Option<&'a IndexEntry> {
        let i = self.by_pkgbase.get(pkgbase)?;
        idx.entries.get(*i as usize)
    }

    /// Resolve a reference to its primary entry from raw user input.
    /// Order matches `classify`: pkgname → provides → pkgbase. The
    /// pkgbase fallback lets `-Si bisq` find an entry whose only pkgname
    /// is `bisq-desktop`. `target` is `&str` because CLI argv is by
    /// definition unclassified — aurox doesn't know if the user typed
    /// a pkgname, a virtual, or a pkgbase.
    pub fn lookup<'a>(&self, idx: &'a IndexFile, target: &str) -> Option<&'a IndexEntry> {
        let bare = strip_version_constraint(target);
        if let Some(i) = self.by_name.get(bare) {
            return idx.entries.get(*i as usize);
        }
        if let Some(providers) = self.by_provides.get(bare)
            && let Some(i) = providers.first()
        {
            return idx.entries.get(*i as usize);
        }
        let i = self.by_pkgbase.get(bare)?;
        idx.entries.get(*i as usize)
    }

    /// Linear regex search across pkgname + pkgdesc, parallelised over entries.
    pub fn search<'a>(&self, idx: &'a IndexFile, regexes: &[regex::Regex]) -> Vec<&'a IndexEntry> {
        idx.entries
            .par_iter()
            .filter(|e| regexes.iter().all(|r| entry_matches(e, r)))
            .collect()
    }
}

fn entry_matches(e: &IndexEntry, r: &regex::Regex) -> bool {
    e.pkgnames
        .iter()
        .any(|p| p.name.matches_regex(r) || p.pkgdesc.as_deref().is_some_and(|d| r.is_match(d)))
        || e.pkgdesc.as_deref().is_some_and(|d| r.is_match(d))
        || e.all_provides().any(|p| p.matches_regex(r))
}

/// Strip pacman dep operators (`>=`, `=`, `<`, …) plus the version expression.
pub fn strip_version_constraint(dep: &str) -> &str {
    for op in [">=", "<=", "==", ">", "<", "="] {
        if let Some(idx) = dep.find(op) {
            return dep[..idx].trim();
        }
    }
    dep.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::index::schema::Pkgname;
    use crate::names::PkgTarget;

    /// Construct a pkgbase entry whose `provides` live at the pkgbase level
    /// (apply to every pkgname implicitly — matches the common AUR shape).
    fn mk(pkgbase: &str, names: &[&str], provides: &[&str]) -> IndexEntry {
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
            provides: provides.iter().map(|s| PkgTarget::new(*s)).collect(),
            pkgver: "1".into(),
            pkgrel: "1".into(),
            ..Default::default()
        }
    }

    /// Construct a split pkgbase where exactly one pkgname declares the
    /// given provides — the bisq shape (`bisq-desktop` provides `bisq`).
    fn mk_scoped(
        pkgbase: &str,
        owner: &str,
        owner_provides: &[&str],
        others: &[&str],
    ) -> IndexEntry {
        let mut pkgnames = vec![Pkgname {
            name: owner.into(),
            provides: owner_provides.iter().map(|s| PkgTarget::new(*s)).collect(),
            pkgdesc: None,
        }];
        for o in others {
            pkgnames.push(Pkgname {
                name: (*o).into(),
                provides: Vec::new(),
                pkgdesc: None,
            });
        }
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames,
            pkgver: "1".into(),
            pkgrel: "1".into(),
            ..Default::default()
        }
    }

    fn fixture() -> IndexFile {
        IndexFile {
            entries: vec![
                mk("cower", &["cower"], &["cower"]),
                mk(
                    "mingw-w64-gcc",
                    &["mingw-w64-gcc", "mingw-w64-gcc-libs"],
                    &[],
                ),
                mk("paru-bin", &["paru-bin"], &["paru=2.0.0"]),
            ],
            ..IndexFile::empty()
        }
    }

    #[test]
    fn strip_constraints() {
        assert_eq!(strip_version_constraint("foo>=1.2"), "foo");
        assert_eq!(strip_version_constraint("foo<=2"), "foo");
        assert_eq!(strip_version_constraint("foo=3"), "foo");
        assert_eq!(strip_version_constraint("foo"), "foo");
    }

    #[test]
    fn split_pkg_maps_all_names() {
        let idx = fixture();
        let s = Lookup::build(&idx);
        assert!(s.by_name.contains_key("mingw-w64-gcc"));
        assert!(s.by_name.contains_key("mingw-w64-gcc-libs"));
        assert_eq!(
            s.by_name["mingw-w64-gcc"], s.by_name["mingw-w64-gcc-libs"],
            "both pkgnames must point to the same pkgbase entry"
        );
    }

    #[test]
    fn lookup_falls_back_to_provides() {
        let idx = fixture();
        let s = Lookup::build(&idx);
        let e = s.lookup(&idx, "paru").expect("provides lookup");
        assert_eq!(e.pkgbase, "paru-bin");
    }

    #[test]
    fn lookup_strips_constraint() {
        let idx = fixture();
        let s = Lookup::build(&idx);
        assert_eq!(s.lookup(&idx, "cower>=10").unwrap().pkgbase, "cower");
    }

    #[test]
    fn by_pkgbase_populated_even_when_no_pkgname_matches() {
        // pkgbase `bisq`, pkgname `bisq-desktop` — the real-world case the
        // by_pkgbase fallback was added for.
        let idx = IndexFile {
            entries: vec![mk("bisq", &["bisq-desktop"], &[])],
            ..IndexFile::empty()
        };
        let s = Lookup::build(&idx);
        assert!(
            !s.by_name.contains_key("bisq"),
            "pkgbase must not leak into by_name"
        );
        assert_eq!(s.by_pkgbase.get("bisq").copied(), Some(0));
        assert_eq!(s.by_name.get("bisq-desktop").copied(), Some(0));
    }

    #[test]
    fn by_pkgbase_covers_all_entries() {
        let idx = fixture();
        let s = Lookup::build(&idx);
        for (i, e) in idx.entries.iter().enumerate() {
            let i = u32::try_from(i).unwrap();
            assert_eq!(s.by_pkgbase.get(&e.pkgbase).copied(), Some(i));
        }
    }

    #[test]
    fn provider_of_picks_the_pkgname_that_declares_the_provides() {
        // bisq shape: one of three pkgnames declares `provides = bisq`.
        // The resolver depends on this exact attribution so `-S bisq`
        // rewrites to `bisq-desktop`, not the whole pkgbase.
        let idx = IndexFile {
            entries: vec![mk_scoped(
                "bisq",
                "bisq-desktop",
                &["bisq"],
                &["bisq-cli", "bisq-daemon"],
            )],
            ..IndexFile::empty()
        };
        let s = Lookup::build(&idx);
        let (entry_idx, pkgname) = s.provider_of(&idx, "bisq").expect("provider lookup");
        assert_eq!(entry_idx, 0);
        assert_eq!(pkgname, "bisq-desktop");
    }

    #[test]
    fn provider_of_handles_pkgbase_level_provides_deterministically() {
        // `pkgbase = mypkg`, pkgbase-level `provides = virtual` — every
        // pkgname provides it implicitly, so we return the first pkgname
        // for a stable answer.
        let idx = IndexFile {
            entries: vec![mk("mypkg", &["mypkg", "mypkg-extras"], &["virtual"])],
            ..IndexFile::empty()
        };
        let s = Lookup::build(&idx);
        let (entry_idx, pkgname) = s.provider_of(&idx, "virtual").expect("provider lookup");
        assert_eq!(entry_idx, 0);
        assert_eq!(
            pkgname, "mypkg",
            "first pkgname is the canonical provider for pkgbase-level provides",
        );
    }

    #[test]
    fn provider_of_strips_version_constraint() {
        // `paru-bin` declares `provides = paru=2.0.0`; users may type
        // `paru>=1` and expect the same provider attribution.
        let idx = fixture();
        let s = Lookup::build(&idx);
        let hit = s.provider_of(&idx, "paru>=1").expect("provider lookup");
        assert_eq!(hit.1, "paru-bin");
    }

    #[test]
    fn provider_of_returns_none_when_no_provides_match() {
        let idx = fixture();
        let s = Lookup::build(&idx);
        assert!(s.provider_of(&idx, "nothing-provides-this").is_none());
    }

    #[test]
    fn search_filters_by_regex() {
        let idx = fixture();
        let s = Lookup::build(&idx);
        let re = regex::Regex::new("ming[wx]").unwrap();
        let hits = s.search(&idx, &[re]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pkgbase, "mingw-w64-gcc");
    }
}
