//! Detect built `.pkg.tar.zst` files and derive their pkgnames.
//!
//! The actual `pacman -U` invocation is driven from `build::mod.rs` so that
//! all sudo work can be batched into a single prompt at the end of the run.

use crate::error::{Error, Result};
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
pub fn extract_pkgname(path: &Path) -> Option<String> {
    let stem = path.file_name()?.to_str()?;
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() < 4 {
        return None;
    }
    let name_parts = &parts[..parts.len() - 3];
    Some(name_parts.join("-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_simple_pkgname() {
        let p = Path::new("/x/cower-17-2-x86_64.pkg.tar.zst");
        assert_eq!(extract_pkgname(p).as_deref(), Some("cower"));
    }

    #[test]
    fn extract_split_pkgname() {
        let p = Path::new("/x/mingw-w64-gcc-libs-13.2.0-1-x86_64.pkg.tar.zst");
        assert_eq!(extract_pkgname(p).as_deref(), Some("mingw-w64-gcc-libs"));
    }

    #[test]
    fn extract_xz_suffix() {
        let p = Path::new("/x/foo-1-1-any.pkg.tar.xz");
        assert_eq!(extract_pkgname(p).as_deref(), Some("foo"));
    }

    #[test]
    fn rejects_too_short() {
        let p = Path::new("/x/foo.pkg.tar.zst");
        assert!(extract_pkgname(p).is_none());
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
