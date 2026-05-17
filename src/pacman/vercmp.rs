//! Wrapper over `alpm_pkg_vercmp` for version ordering.

use std::cmp::Ordering;

/// Compare two pacman-style version strings (`epoch:pkgver-pkgrel`).
pub fn vercmp(a: &str, b: &str) -> Ordering {
    alpm::vercmp(a, b)
}

/// True if `installed` is strictly older than `available`.
pub fn is_outdated(installed: &str, available: &str) -> bool {
    vercmp(installed, available) == Ordering::Less
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn basic_ordering() {
        assert_eq!(vercmp("1.0", "1.0"), Ordering::Equal);
        assert_eq!(vercmp("1.0", "1.1"), Ordering::Less);
        assert_eq!(vercmp("2.0", "1.9"), Ordering::Greater);
    }

    #[test]
    fn pkgrel_breaks_ties() {
        assert_eq!(vercmp("1.0-1", "1.0-2"), Ordering::Less);
        assert_eq!(vercmp("1.0-3", "1.0-2"), Ordering::Greater);
    }

    #[test]
    fn epoch_dominates() {
        assert_eq!(vercmp("1:1.0", "999.0"), Ordering::Greater);
    }

    #[test]
    fn outdated_helper() {
        assert!(is_outdated("1.0-1", "1.0-2"));
        assert!(!is_outdated("1.0-2", "1.0-2"));
        assert!(!is_outdated("1.1", "1.0"));
    }
}
