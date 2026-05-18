//! Secondary lookup tables built after loading the primary index.

use crate::index::schema::{IndexEntry, IndexFile};
use rayon::prelude::*;
use smallvec::SmallVec;
use std::collections::HashMap;
use tracing::{debug, instrument};

/// Post-load lookup structure: pkgname / provides / pkgbase → index position.
pub struct Secondary {
    /// pkgname → entries[idx]. Split pkgs map multiple names to the same idx.
    pub by_name: HashMap<String, u32>,
    /// `provides` virtual name → set of entry indices.
    pub by_provides: HashMap<String, SmallVec<[u32; 2]>>,
    /// pkgbase → entries[idx]. Used as a last-resort lookup so users can write
    /// `-S <pkgbase>` (yay-style) even when no pkgname equals the pkgbase —
    /// e.g. pkgbase `bisq` produces pkgname `bisq-desktop`. Resolution order
    /// elsewhere keeps pkgname / provides preferred over pkgbase.
    pub by_pkgbase: HashMap<String, u32>,
}

impl Secondary {
    /// Build `by_name`, `by_provides`, and `by_pkgbase` over the loaded index.
    #[instrument(skip(idx), fields(entries = idx.entries.len()))]
    pub fn build(idx: &IndexFile) -> Self {
        let mut by_name = HashMap::with_capacity(idx.entries.len() * 2);
        let mut by_provides: HashMap<String, SmallVec<[u32; 2]>> = HashMap::new();
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
                let base = strip_version_constraint(prov);
                by_provides.entry(base.to_string()).or_default().push(i);
            }
            by_pkgbase.insert(e.pkgbase.clone(), i);
        }
        debug!(
            by_name = by_name.len(),
            by_provides = by_provides.len(),
            by_pkgbase = by_pkgbase.len(),
            "secondary indexes built"
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
    pub fn provider_of<'a>(
        &self,
        idx: &'a IndexFile,
        name: &str,
    ) -> Option<(usize, &'a str)> {
        let bare = strip_version_constraint(name);
        let &entry_idx = self.by_provides.get(bare)?.first()?;
        let entry = idx.entries.get(entry_idx as usize)?;
        // Prefer the pkgname that explicitly declared this provides; that's
        // the case the bisq/yay parity work was added for.
        for pkg in &entry.pkgnames {
            if pkg
                .provides
                .iter()
                .any(|p| strip_version_constraint(p) == bare)
            {
                return Some((entry_idx as usize, pkg.name.as_str()));
            }
        }
        // No pkgname owned it, so the match came from a pkgbase-level
        // provides — every pkgname provides it implicitly. Pick the first
        // for a deterministic answer.
        entry
            .pkgnames
            .first()
            .map(|p| (entry_idx as usize, p.name.as_str()))
    }

    /// Resolve a reference to its primary entry. Order matches `classify`:
    /// pkgname → provides → pkgbase. The pkgbase fallback lets `-Si bisq`
    /// find an entry whose only pkgname is `bisq-desktop`.
    pub fn lookup<'a>(&self, idx: &'a IndexFile, target: &str) -> Option<&'a IndexEntry> {
        let bare = strip_version_constraint(target);
        if let Some(i) = self.by_name.get(bare) {
            return idx.entries.get(*i as usize);
        }
        if let Some(providers) = self.by_provides.get(bare) {
            if let Some(i) = providers.first() {
                return idx.entries.get(*i as usize);
            }
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
    e.pkgnames.iter().any(|p| r.is_match(&p.name))
        || e.pkgdesc.as_deref().is_some_and(|d| r.is_match(d))
        || e.all_provides().any(|p| r.is_match(p))
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
                })
                .collect(),
            provides: provides.iter().map(|s| (*s).into()).collect(),
            pkgver: "1".into(),
            pkgrel: "1".into(),
            ..Default::default()
        }
    }

    /// Construct a split pkgbase where exactly one pkgname declares the
    /// given provides — the bisq shape (`bisq-desktop` provides `bisq`).
    fn mk_scoped(pkgbase: &str, owner: &str, owner_provides: &[&str], others: &[&str]) -> IndexEntry {
        let mut pkgnames = vec![Pkgname {
            name: owner.into(),
            provides: owner_provides.iter().map(|s| (*s).into()).collect(),
        }];
        for o in others {
            pkgnames.push(Pkgname {
                name: (*o).into(),
                provides: Vec::new(),
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
        let s = Secondary::build(&idx);
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
        let s = Secondary::build(&idx);
        let e = s.lookup(&idx, "paru").expect("provides lookup");
        assert_eq!(e.pkgbase, "paru-bin");
    }

    #[test]
    fn lookup_strips_constraint() {
        let idx = fixture();
        let s = Secondary::build(&idx);
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
        let s = Secondary::build(&idx);
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
        let s = Secondary::build(&idx);
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
        let s = Secondary::build(&idx);
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
        let s = Secondary::build(&idx);
        assert_eq!(
            s.provider_of(&idx, "virtual"),
            Some((0, "mypkg")),
            "first pkgname is the canonical provider for pkgbase-level provides",
        );
    }

    #[test]
    fn provider_of_strips_version_constraint() {
        // `paru-bin` declares `provides = paru=2.0.0`; users may type
        // `paru>=1` and expect the same provider attribution.
        let idx = fixture();
        let s = Secondary::build(&idx);
        let hit = s.provider_of(&idx, "paru>=1").expect("provider lookup");
        assert_eq!(hit.1, "paru-bin");
    }

    #[test]
    fn provider_of_returns_none_when_no_provides_match() {
        let idx = fixture();
        let s = Secondary::build(&idx);
        assert!(s.provider_of(&idx, "nothing-provides-this").is_none());
    }

    #[test]
    fn search_filters_by_regex() {
        let idx = fixture();
        let s = Secondary::build(&idx);
        let re = regex::Regex::new("mingw").unwrap();
        let hits = s.search(&idx, &[re]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pkgbase, "mingw-w64-gcc");
    }
}
