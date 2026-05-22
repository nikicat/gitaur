//! Structural parsing + display-oriented diffing of Arch package versions.
//!
//! Arch versions follow `[epoch:]pkgver-pkgrel`, where pkgver is arbitrary
//! upstream text. The `semver` crate would reject most real-world pkgvers
//! (`20240101`, `1.0pre1`, `1_0`, `r123.abc`), so we do our own parsing.
//! Pure version ordering lives on [`Ver`]'s `PartialOrd` impl — this
//! module is about *how* two versions differ, for the upgrade-table UI.

use crate::version::Ver;

/// Granularity of a version bump — drives upgrade-table colorization and
/// row ordering.
///
/// Variants are declared **most-severe → least-severe**, and the derived
/// `Ord` reflects that: `Epoch < Major < … < Other`. Sorting a slice of
/// `BumpKind` ascending therefore lists the highest-severity bumps first,
/// which is the order the upgrade table renders rows in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BumpKind {
    /// Epoch (`N:`) changed — a forced ordering override.
    Epoch,
    /// First pkgver component differs.
    Major,
    /// Second pkgver component differs.
    Minor,
    /// Third or later pkgver component differs.
    Patch,
    /// Only the pkgrel (`-N`) trailer changed — a packaging-only respin.
    PkgRel,
    /// No structural difference detected (equal versions or unrecognized scheme).
    Other,
}

/// Split `[epoch:]pkgver-pkgrel` into `(epoch, pkgver, pkgrel)`.
fn split_ver(v: &str) -> (Option<&str>, &str, Option<&str>) {
    let (epoch, rest) = match v.split_once(':') {
        Some((e, r)) => (Some(e), r),
        None => (None, v),
    };
    let (pkgver, pkgrel) = match rest.rsplit_once('-') {
        Some((pv, pr)) => (pv, Some(pr)),
        None => (rest, None),
    };
    (epoch, pkgver, pkgrel)
}

/// Classify the bump tier between two Arch versions for display purposes.
///
/// Compares epoch first, then pkgver split on `.`, then pkgrel. The first
/// differing layer wins, so `1.0-1 → 1.0-2` is [`BumpKind::PkgRel`] even
/// though pkgrel sits after pkgver in the version string.
pub fn classify_bump(old: &Ver, new: &Ver) -> BumpKind {
    let (o_ep, o_pv, o_pr) = split_ver(old.as_str());
    let (n_ep, n_pv, n_pr) = split_ver(new.as_str());

    if o_ep != n_ep {
        return BumpKind::Epoch;
    }
    if o_pv != n_pv {
        for (i, (op, np)) in o_pv.split('.').zip(n_pv.split('.')).enumerate() {
            if op != np {
                return match i {
                    0 => BumpKind::Major,
                    1 => BumpKind::Minor,
                    _ => BumpKind::Patch,
                };
            }
        }
        // Common prefix matches but one side has an extra component
        // (e.g. `1.2 → 1.2.1`). Treat as a patch.
        return BumpKind::Patch;
    }
    if o_pr != n_pr {
        return BumpKind::PkgRel;
    }
    BumpKind::Other
}

/// Find the longest common prefix of `old` and `new` that ends at a
/// non-alphanumeric boundary — i.e. a version-component separator. Same
/// mechanic paru uses (`src/upgrade.rs::get_version_diff`): walk char-by-char,
/// remembering the most recent separator while still inside the common run;
/// when a divergence is hit, back up to that separator so we don't split
/// mid-component. Returns the byte index where the diverging suffix begins.
pub fn common_prefix_at_boundary(old: &Ver, new: &Ver) -> usize {
    let mut last_boundary = 0;
    let mut byte_pos = 0;
    for (a, b) in old.as_str().chars().zip(new.as_str().chars()) {
        if a != b {
            return last_boundary;
        }
        byte_pos += a.len_utf8();
        if !a.is_alphanumeric() {
            last_boundary = byte_pos;
        }
    }
    // One string is a prefix of the other. The shared run is the common
    // prefix in full — nothing to back up.
    byte_pos
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only literal helper. `Ver::new("…")` reads tersely at the
    /// assertion call site; one `v(...)` per arg keeps the line short.
    fn v(s: &str) -> &Ver {
        Ver::new(s)
    }

    #[test]
    fn classify_bump_layers() {
        assert_eq!(classify_bump(v("1.0-1"), v("1.0-2")), BumpKind::PkgRel);
        assert_eq!(classify_bump(v("1.0.0-1"), v("1.0.1-1")), BumpKind::Patch);
        assert_eq!(classify_bump(v("1.0-1"), v("1.1-1")), BumpKind::Minor);
        assert_eq!(classify_bump(v("1.0-1"), v("2.0-1")), BumpKind::Major);
        assert_eq!(classify_bump(v("1:1.0-1"), v("2:1.0-1")), BumpKind::Epoch);
        assert_eq!(classify_bump(v("1.0-1"), v("1.0-1")), BumpKind::Other);
    }

    #[test]
    fn classify_bump_handles_trailing_component() {
        assert_eq!(classify_bump(v("1.2-1"), v("1.2.1-1")), BumpKind::Patch);
    }

    #[test]
    fn classify_bump_first_layer_wins() {
        // pkgrel also changed, but the pkgver bump is what we report.
        assert_eq!(classify_bump(v("1.0-3"), v("1.1-1")), BumpKind::Minor);
    }

    /// `BumpKind`'s derived `Ord` is load-bearing: `upgrade_table` sorts
    /// rows by it to put the most-severe bumps first. If a future refactor
    /// reorders the enum variants this test breaks immediately.
    #[test]
    fn bumpkind_orders_most_severe_first() {
        let mut kinds = [
            BumpKind::Other,
            BumpKind::PkgRel,
            BumpKind::Patch,
            BumpKind::Minor,
            BumpKind::Major,
            BumpKind::Epoch,
        ];
        kinds.sort();
        assert_eq!(
            kinds,
            [
                BumpKind::Epoch,
                BumpKind::Major,
                BumpKind::Minor,
                BumpKind::Patch,
                BumpKind::PkgRel,
                BumpKind::Other,
            ]
        );
    }

    /// Helper: byte index → suffix of `new`. Reads more naturally in tests.
    /// Inputs are `&str` literals; wrap once before calling the typed API
    /// and slice the original `new` by the returned byte position.
    fn suffix_of<'a>(old: &str, new: &'a str) -> &'a str {
        &new[common_prefix_at_boundary(Ver::new(old), Ver::new(new))..]
    }

    #[test]
    fn split_at_component_boundary_not_mid_component() {
        // Char-by-char common prefix is `1.2`, but the second component
        // changed (`2` → `20`) so we back up to the `.` separator.
        assert_eq!(suffix_of("1.2.3", "1.20.0"), "20.0");
    }

    #[test]
    fn split_pkgrel_only() {
        assert_eq!(suffix_of("1.0.0-1", "1.0.0-2"), "2");
    }

    #[test]
    fn split_patch_bump() {
        assert_eq!(suffix_of("1.0.0-1", "1.0.1-1"), "1-1");
    }

    #[test]
    fn split_major_bump_no_prefix() {
        assert_eq!(suffix_of("1.2.3", "2.0.0"), "2.0.0");
    }

    #[test]
    fn split_epoch_change() {
        // Epoch `:` is the first divergence char and is itself a separator,
        // so the suffix starts right at the new epoch digit.
        assert_eq!(suffix_of("1:1.0-1", "2:1.0-1"), "2:1.0-1");
    }

    #[test]
    fn split_extra_trailing_component() {
        // No divergence inside the common run — fall through to keeping the
        // full shared prefix, so the new suffix is just the added tail.
        assert_eq!(suffix_of("1.2", "1.2.1"), ".1");
    }

    #[test]
    fn split_identical_versions() {
        assert_eq!(suffix_of("1.0-1", "1.0-1"), "");
    }

    #[test]
    fn split_handles_underscore_as_boundary() {
        // `1.0_beta1` → `1.0_beta2`: `_` is non-alphanumeric, so we back up
        // to it rather than splitting inside `beta1`/`beta2`.
        assert_eq!(suffix_of("1.0_beta1", "1.0_beta2"), "beta2");
    }

    #[test]
    fn split_alpha_suffix_inside_component() {
        // `5.10rc1` vs `5.10rc2`: no separator between `5.10` and `rc1`,
        // so we back up to the `.` and color the whole `10rc2`.
        assert_eq!(suffix_of("5.10rc1", "5.10rc2"), "10rc2");
    }

    #[test]
    fn split_is_utf8_safe() {
        // Multi-byte chars in the common run must not desync byte indexing.
        assert_eq!(suffix_of("1.α-1", "1.α-2"), "2");
    }
}
