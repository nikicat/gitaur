//! Line-oriented parser for `.SRCINFO` files (a flat `key = value` dump
//! produced by `makepkg --printsrcinfo`).
//!
//! Arch-suffixed list keys (`depends_x86_64`, `provides_aarch64`, …) are
//! folded into their canonical base name — gitaur doesn't need per-arch
//! resolution for index lookups.

use crate::error::{Error, Result};
use crate::index::schema::IndexEntry;
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
pub fn parse(text: &str) -> Result<IndexEntry> {
    let mut e = IndexEntry::default();
    // Every .SRCINFO opens with the pkgbase scalar section; once we see
    // the first `pkgname =` line, later scalars belong to that split member
    // and we stop overriding pkgbase-level scalars.
    let mut in_pkgbase = true;
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
                in_pkgbase = true;
            }
            "pkgname" => {
                e.pkgnames.push(v.into());
                in_pkgbase = false;
            }

            "pkgver" if in_pkgbase => e.pkgver = v.into(),
            "pkgrel" if in_pkgbase => e.pkgrel = v.into(),
            "epoch" if in_pkgbase => e.epoch = Some(v.into()),
            "pkgdesc" if in_pkgbase => e.pkgdesc = Some(v.into()),

            "arch" => e.arch.push(v.into()),
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
        e.pkgnames.push(e.pkgbase.clone());
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
        &mut e.pkgnames,
    ] {
        dedup(v);
    }
    Ok(e)
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

/// Lookup the `&mut Vec<String>` for one of the array-valued keys.
fn list_field_mut<'a>(e: &'a mut IndexEntry, key: &str) -> &'a mut Vec<String> {
    match key {
        "depends" => &mut e.depends,
        "makedepends" => &mut e.makedepends,
        "checkdepends" => &mut e.checkdepends,
        "optdepends" => &mut e.optdepends,
        "provides" => &mut e.provides,
        "conflicts" => &mut e.conflicts,
        "replaces" => &mut e.replaces,
        _ => unreachable!("ARRAY_KEYS membership checked before call"),
    }
}

fn dedup(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|s| seen.insert(s.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    const COWER: &str = r"
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

    const SPLIT: &str = r"
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

    #[test]
    fn parses_simple() {
        let e = parse(COWER).unwrap();
        assert_eq!(e.pkgbase, "cower");
        assert_eq!(e.pkgver, "17");
        assert_eq!(e.pkgrel, "2");
        assert_eq!(e.pkgnames, vec!["cower"]);
        assert!(e.depends.contains(&"pacman".to_string()));
        assert!(e.depends.contains(&"curl".to_string()));
        assert!(e.provides.contains(&"cower".to_string()));
        assert!(e.conflicts.contains(&"cower-git".to_string()));
        assert!(e.arch.contains(&"x86_64".to_string()));
    }

    #[test]
    fn parses_split() {
        let e = parse(SPLIT).unwrap();
        assert_eq!(e.pkgbase, "mingw-w64-gcc");
        assert_eq!(
            e.pkgnames,
            vec![
                "mingw-w64-gcc".to_string(),
                "mingw-w64-gcc-libs".to_string(),
                "mingw-w64-gcc-fortran".to_string()
            ]
        );
        // Pkgbase-level + pkgname-level depends are both collected.
        assert!(e.depends.contains(&"mingw-w64-crt".to_string()));
        assert!(e.depends.contains(&"mingw-w64-winpthreads".to_string()));
    }

    #[test]
    fn arch_suffixed_keys() {
        let s = "pkgbase = foo\npkgver = 1\npkgrel = 1\npkgname = foo\ndepends_x86_64 = libfoo\nprovides_aarch64 = bar\n";
        let e = parse(s).unwrap();
        assert!(e.depends.contains(&"libfoo".to_string()));
        assert!(e.provides.contains(&"bar".to_string()));
    }

    #[test]
    fn rejects_missing_pkgbase() {
        let s = "pkgver = 1\npkgrel = 1\n";
        assert!(parse(s).is_err());
    }
}
