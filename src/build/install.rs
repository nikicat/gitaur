//! Detect built `.pkg.tar.zst` files and derive their pkgnames.
//!
//! The actual `pacman -U` invocation is driven from `build::mod.rs` so that
//! all sudo work can be batched into a single prompt at the end of the run.

use crate::error::{Error, Result};
use crate::names::PkgName;
use crate::version::Ver;
use glob::glob;
use std::path::{Path, PathBuf};

/// Glob `<worktree>/*.pkg.tar.{zst,xz}` and return all matches.
pub fn find_produced(worktree: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for pat in ["*.pkg.tar.zst", "*.pkg.tar.xz"] {
        let glob_pat = worktree.join(pat);
        for p in glob(&glob_pat.to_string_lossy())
            .map_err(|e| Error::other(e.to_string()))?
            .flatten()
        {
            out.push(p);
        }
    }
    Ok(out)
}

/// Best-effort pkgname extraction from a package filename.
/// Format: `<pkgname>-<pkgver>-<pkgrel>-<arch>.pkg.tar.{zst,xz}`.
pub fn extract_pkgname(path: &Path) -> Option<PkgName> {
    let stem = path.file_name()?.to_str()?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 4 {
        return None;
    }
    let name_parts = &parts[..parts.len() - 3];
    Some(PkgName::new(name_parts.join("-")))
}

/// True iff `path`'s filename is `<pkgname>-<version>-<arch>.pkg.tar.{zst,xz}`
/// — i.e. an artifact this exact `(pkgname, version)` would produce.
/// `version` is the pacman-style `[epoch:]pkgver-pkgrel`, accepted as
/// `&Ver` so callers can pass the typed `Version` directly. This match is
/// what powers the build idempotency check. `pkgname` is the typed
/// `PkgName` (matched against the filename prefix via `Display`).
pub fn matches_pkg(path: &Path, pkgname: &PkgName, version: &Ver) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if !(name.ends_with(".pkg.tar.zst") || name.ends_with(".pkg.tar.xz")) {
        return false;
    }
    // Match the pkgname prefix via the wrapped String — `PkgName` deliberately
    // doesn't expose `as_str`, so `strip_prefix` on the inner is the
    // dedicated read path for filename comparisons.
    let pkgname_str = pkgname.to_string();
    let Some(rest) = name
        .strip_prefix(&pkgname_str)
        .and_then(|r| r.strip_prefix('-'))
    else {
        return false;
    };
    let Some(after_ver) = version.strip_filename_segment(rest) else {
        return false;
    };
    // What's left is `<arch>.pkg.tar.{zst,xz}`; arch must not be empty and
    // must not contain another '-' (would mean we matched a longer pkgname
    // or a prefix of pkgver by accident).
    let dot = after_ver.find('.').unwrap_or(after_ver.len());
    let arch = &after_ver[..dot];
    !arch.is_empty() && !arch.contains('-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_simple_pkgname() {
        let p = Path::new("/x/cower-17-2-x86_64.pkg.tar.zst");
        assert_eq!(extract_pkgname(p), Some(PkgName::new("cower")));
    }

    #[test]
    fn extract_split_pkgname() {
        let p = Path::new("/x/mingw-w64-gcc-libs-13.2.0-1-x86_64.pkg.tar.zst");
        assert_eq!(extract_pkgname(p), Some(PkgName::new("mingw-w64-gcc-libs")));
    }

    #[test]
    fn extract_xz_suffix() {
        let p = Path::new("/x/foo-1-1-any.pkg.tar.xz");
        assert_eq!(extract_pkgname(p), Some(PkgName::new("foo")));
    }

    #[test]
    fn rejects_too_short() {
        let p = Path::new("/x/foo.pkg.tar.zst");
        assert!(extract_pkgname(p).is_none());
    }

    fn pn(s: &str) -> PkgName {
        PkgName::new(s)
    }

    /// `&Ver` literal helper. Mirrors `pn` for terse test call sites.
    fn v(s: &str) -> &Ver {
        Ver::new(s)
    }

    #[test]
    fn matches_pkg_exact() {
        let p = Path::new("/x/cower-17-2-x86_64.pkg.tar.zst");
        assert!(matches_pkg(p, &pn("cower"), v("17-2")));
        assert!(!matches_pkg(p, &pn("cower"), v("17-1")));
        assert!(!matches_pkg(p, &pn("cower"), v("18-2")));
        assert!(!matches_pkg(p, &pn("cower-bin"), v("17-2")));
    }

    #[test]
    fn matches_pkg_with_epoch() {
        let p = Path::new("/x/foo-2:1.0-1-x86_64.pkg.tar.zst");
        assert!(matches_pkg(p, &pn("foo"), v("2:1.0-1")));
        assert!(!matches_pkg(p, &pn("foo"), v("1.0-1")));
    }

    #[test]
    fn matches_pkg_split_pkgname() {
        let p = Path::new("/x/mingw-w64-gcc-libs-13.2.0-1-x86_64.pkg.tar.zst");
        assert!(matches_pkg(p, &pn("mingw-w64-gcc-libs"), v("13.2.0-1")));
        // Wrong pkgname: matches as prefix but arch slot would contain a '-'.
        assert!(!matches_pkg(p, &pn("mingw-w64-gcc"), v("libs-13.2.0")));
    }

    #[test]
    fn matches_pkg_xz_suffix() {
        let p = Path::new("/x/foo-1.0-1-any.pkg.tar.xz");
        assert!(matches_pkg(p, &pn("foo"), v("1.0-1")));
    }

    #[test]
    fn matches_pkg_rejects_wrong_suffix() {
        let p = Path::new("/x/foo-1.0-1-any.pkg.tar.gz");
        assert!(!matches_pkg(p, &pn("foo"), v("1.0-1")));
    }

    #[test]
    fn find_produced_globs_zst_and_xz() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a-1-1-x86_64.pkg.tar.zst"), "").unwrap();
        std::fs::write(dir.path().join("b-1-1-any.pkg.tar.xz"), "").unwrap();
        std::fs::write(dir.path().join("PKGBUILD"), "").unwrap();
        let mut found = find_produced(dir.path()).unwrap();
        found.sort();
        let names: Vec<String> = found
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["a-1-1-x86_64.pkg.tar.zst", "b-1-1-any.pkg.tar.xz"]
        );
    }
}
