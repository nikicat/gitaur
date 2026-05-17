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
            for name in &e.pkgnames {
                by_name.insert(name.clone(), i);
            }
            for prov in &e.provides {
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
    e.pkgnames.iter().any(|n| r.is_match(n))
        || e.pkgdesc.as_deref().is_some_and(|d| r.is_match(d))
        || e.provides.iter().any(|p| r.is_match(p))
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

    fn mk(pkgbase: &str, names: &[&str], provides: &[&str]) -> IndexEntry {
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: names.iter().map(|s| (*s).into()).collect(),
            provides: provides.iter().map(|s| (*s).into()).collect(),
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
    fn search_filters_by_regex() {
        let idx = fixture();
        let s = Secondary::build(&idx);
        let re = regex::Regex::new("mingw").unwrap();
        let hits = s.search(&idx, &[re]);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].pkgbase, "mingw-w64-gcc");
    }
}
