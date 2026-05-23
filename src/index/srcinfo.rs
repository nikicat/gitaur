//! Line-oriented parser for `.SRCINFO` files (a flat `key = value` dump
//! produced by `makepkg --printsrcinfo`).
//!
//! Arch-suffixed list keys (`depends_x86_64`, `provides_aarch64`, …) are
//! folded into their canonical base name — gitaur doesn't need per-arch
//! resolution for index lookups.

use crate::error::{Error, Result};
use crate::index::schema::{IndexEntry, Pkgname};
use tracing::trace;

/// Array-valued keys that may carry an arch suffix (`<key>_<arch>`).
const ARRAY_KEYS: &[&str] = &[
    "depends",
    "makedepends",
    "checkdepends",
    "optdepends",
    "provides",
    "conflicts",
    "replaces",
];

/// Parse a `.SRCINFO` blob into an [`IndexEntry`]. The caller fills
/// `commit_oid` and `srcinfo_blob_oid` after locating the source blob.
///
/// `provides = …` lines are attributed by section:
///   * lines before any `pkgname = …` land in `e.provides` (pkgbase-level —
///     apply to every pkgname implicitly).
///   * lines inside a `pkgname = X` section land on that `Pkgname.provides`,
///     letting the resolver pick the right split-package member when the
///     user types a virtual name like `bisq` that only `bisq-desktop`
///     provides.
pub fn parse(text: &str) -> Result<IndexEntry> {
    let mut e = IndexEntry::default();
    // -1 while in the pkgbase header; switches to the most recent
    // `pkgname = …` index for the rest of the file.
    let mut current_pkgname: Option<usize> = None;
    let mut saw_anything = false;

    for (lineno, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=').map(|(k, v)| (k.trim(), v.trim())) else {
            return Err(Error::SrcInfo(format!("line {}: no `=`", lineno + 1)));
        };
        saw_anything = true;

        match canonical(k) {
            "pkgbase" => {
                e.pkgbase = v.into();
                current_pkgname = None;
            }
            "pkgname" => {
                e.pkgnames.push(Pkgname {
                    name: v.into(),
                    provides: Vec::new(),
                });
                current_pkgname = Some(e.pkgnames.len() - 1);
            }

            "pkgver" if current_pkgname.is_none() => e.pkgver = v.into(),
            "pkgrel" if current_pkgname.is_none() => e.pkgrel = v.into(),
            "epoch" if current_pkgname.is_none() => e.epoch = Some(v.into()),
            "pkgdesc" if current_pkgname.is_none() => e.pkgdesc = Some(v.into()),

            "arch" => e.arch.push(v.into()),
            // `provides` gets attribution: pkgbase-level vs pkgname-scoped.
            // Every other array key stays pkgbase-flat — we don't (yet) need
            // pkgname-level depends/conflicts/replaces for resolution.
            "provides" => match current_pkgname {
                None => e.provides.push(v.into()),
                Some(i) => e.pkgnames[i].provides.push(v.into()),
            },
            list_key if ARRAY_KEYS.contains(&list_key) => {
                list_field_mut(&mut e, list_key).push(v.into());
            }

            _ => trace!(key = k, "ignored .SRCINFO key"),
        }
    }

    if !saw_anything {
        return Err(Error::SrcInfo("empty .SRCINFO".into()));
    }
    if e.pkgbase.is_empty() {
        return Err(Error::SrcInfo("missing pkgbase".into()));
    }
    if e.pkgnames.is_empty() {
        // No `pkgname = …` lines → Arch semantics say pkgname defaults to
        // pkgbase. `PkgBase::canonical_pkgname` is the dedicated method
        // for this exact case (see its doc-comment for the narrow valid
        // uses).
        e.pkgnames.push(Pkgname {
            name: e.pkgbase.canonical_pkgname(),
            provides: Vec::new(),
        });
    }
    for v in [
        &mut e.depends,
        &mut e.makedepends,
        &mut e.checkdepends,
        &mut e.optdepends,
        &mut e.provides,
        &mut e.conflicts,
        &mut e.replaces,
        &mut e.arch,
    ] {
        dedup(v);
    }
    // Dedupe per-pkgname provides; also collapse duplicate pkgname entries
    // (rare malformed .SRCINFOs ship the same `pkgname = X` twice).
    for p in &mut e.pkgnames {
        dedup(&mut p.provides);
    }
    dedup_pkgnames(&mut e.pkgnames);
    Ok(e)
}

fn dedup_pkgnames(v: &mut Vec<Pkgname>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|p| seen.insert(p.name.clone()));
}

/// Fold `<base>_<arch>` keys onto `<base>`; pass everything else through.
fn canonical(k: &str) -> &str {
    if let Some(under) = k.find('_') {
        let base = &k[..under];
        if ARRAY_KEYS.contains(&base) {
            return base;
        }
    }
    k
}

/// Lookup the `&mut Vec<String>` for one of the array-valued keys. `provides`
/// is handled in the match above (it routes by section, not by key), so it
/// is intentionally absent here.
fn list_field_mut<'a>(e: &'a mut IndexEntry, key: &str) -> &'a mut Vec<String> {
    match key {
        "depends" => &mut e.depends,
        "makedepends" => &mut e.makedepends,
        "checkdepends" => &mut e.checkdepends,
        "optdepends" => &mut e.optdepends,
        "conflicts" => &mut e.conflicts,
        "replaces" => &mut e.replaces,
        _ => unreachable!("ARRAY_KEYS membership and provides routing checked before call"),
    }
}

fn dedup(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|s| seen.insert(s.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::PkgName;

    const COWER: &str = "
pkgbase = cower
	pkgdesc = A simple AUR agent with a pretentious name
	pkgver = 17
	pkgrel = 2
	url = https://github.com/falconindy/cower
	arch = i686
	arch = x86_64
	makedepends = perl
	depends = pacman
	depends = curl
	depends = yajl
	provides = cower
	conflicts = cower-git

pkgname = cower
";

    const SPLIT: &str = "
pkgbase = mingw-w64-gcc
	pkgver = 13.2.0
	pkgrel = 1
	makedepends = mingw-w64-binutils
	depends = mingw-w64-crt

pkgname = mingw-w64-gcc
	depends = mingw-w64-winpthreads

pkgname = mingw-w64-gcc-libs
	pkgdesc = Runtime libs

pkgname = mingw-w64-gcc-fortran
";

    /// 3-way split where one pkgname declares `provides`. Mirrors the real
    /// AUR `bisq` entry, which is the regression target for per-pkgname
    /// provides attribution.
    const BISQ: &str = "
pkgbase = bisq
	pkgver = 1.9.22
	pkgrel = 2
	makedepends = jdk11-openjdk
	depends = jdk11-openjdk

pkgname = bisq-desktop
	provides = bisq
	conflicts = bisq-bin

pkgname = bisq-cli

pkgname = bisq-daemon
";

    fn names(e: &IndexEntry) -> Vec<&PkgName> {
        e.pkgnames.iter().map(|p| &p.name).collect()
    }

    #[test]
    fn parses_simple() {
        let e = parse(COWER).unwrap();
        assert_eq!(e.pkgbase, "cower");
        assert_eq!(e.pkgver, "17");
        assert_eq!(e.pkgrel, "2");
        assert_eq!(names(&e), vec!["cower"]);
        assert!(e.depends.contains(&"pacman".to_owned()));
        assert!(e.depends.contains(&"curl".to_owned()));
        // Pkgbase-level provides land on the entry, not on the pkgname.
        assert!(e.provides.contains(&"cower".to_owned()));
        assert!(e.pkgnames[0].provides.is_empty());
        assert!(e.conflicts.contains(&"cower-git".to_owned()));
        assert!(e.arch.contains(&"x86_64".to_owned()));
    }

    #[test]
    fn parses_split() {
        let e = parse(SPLIT).unwrap();
        assert_eq!(e.pkgbase, "mingw-w64-gcc");
        assert_eq!(
            names(&e),
            vec![
                "mingw-w64-gcc",
                "mingw-w64-gcc-libs",
                "mingw-w64-gcc-fortran"
            ]
        );
        // Pkgbase-level + pkgname-level depends are both collected.
        assert!(e.depends.contains(&"mingw-w64-crt".to_owned()));
        assert!(e.depends.contains(&"mingw-w64-winpthreads".to_owned()));
    }

    #[test]
    fn pkgname_scoped_provides_lands_on_the_right_pkgname() {
        // The bisq regression: only `bisq-desktop` declares `provides = bisq`;
        // the resolver needs to find it on that pkgname's slot, not on the
        // pkgbase-level list or on the wrong sibling.
        let e = parse(BISQ).unwrap();
        assert_eq!(names(&e), vec!["bisq-desktop", "bisq-cli", "bisq-daemon"]);
        assert!(
            e.provides.is_empty(),
            "no pkgbase-level provides → e.provides must be empty",
        );
        assert_eq!(e.pkgnames[0].provides, vec!["bisq".to_owned()]);
        assert!(e.pkgnames[1].provides.is_empty());
        assert!(e.pkgnames[2].provides.is_empty());
        // all_provides() unions both buckets, regardless of attribution.
        let all: Vec<&str> = e.all_provides().collect();
        assert_eq!(all, vec!["bisq"]);
    }

    #[test]
    fn pkgbase_provides_inherited_implicitly() {
        // When `provides` is declared at the pkgbase level, every pkgname is
        // a provider — we encode that by leaving the per-pkgname slots
        // empty and letting `e.provides` carry the line. Resolution code
        // that walks both via `all_provides()` (or queries Secondary) sees
        // the same lookup either way.
        let s = "pkgbase = foo\npkgver = 1\npkgrel = 1\nprovides = bar\npkgname = foo\npkgname = foo-extras\n";
        let e = parse(s).unwrap();
        assert_eq!(e.provides, vec!["bar".to_owned()]);
        for p in &e.pkgnames {
            assert!(p.provides.is_empty());
        }
    }

    #[test]
    fn arch_suffixed_keys() {
        // `provides_<arch>` declared at the pkgname level must still land on
        // that pkgname, not on the pkgbase. Catches a regression where
        // arch-folding bypassed the per-section routing.
        let s = "pkgbase = foo\npkgver = 1\npkgrel = 1\npkgname = foo\ndepends_x86_64 = libfoo\nprovides_aarch64 = bar\n";
        let e = parse(s).unwrap();
        assert!(e.depends.contains(&"libfoo".to_owned()));
        assert_eq!(e.pkgnames[0].provides, vec!["bar".to_owned()]);
        assert!(e.provides.is_empty());
    }

    #[test]
    fn rejects_missing_pkgbase() {
        let s = "pkgver = 1\npkgrel = 1\n";
        assert!(parse(s).is_err());
    }
}
