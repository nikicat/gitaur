//! On-disk index schema. Persisted via `rkyv 0.8` zero-copy archive.

use crate::names::{PkgBase, PkgName};
use crate::version::Version;
use rkyv::{Archive, Deserialize, Serialize};

/// One pkgname's metadata inside a pkgbase. Split-package PKGBUILDs override
/// some fields per-pkgname; pkgbase-level fields on [`IndexEntry`] apply to
/// every pkgname implicitly and are not duplicated here.
///
/// Today this only carries pkgname-scoped `provides`, because that's the one
/// field gitaur's resolver needs to disambiguate split packages (e.g. yay-
/// style `-S bisq` matching `bisq-desktop`'s `provides=bisq`, not the other
/// two siblings in the same pkgbase). Pkgname-scoped `depends`, `conflicts`,
/// `replaces` etc. can be added the same way if a future feature needs them.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Pkgname {
    /// The pkgname itself.
    pub name: PkgName,
    /// `provides = …` declared inside this pkgname's section in `.SRCINFO`.
    /// Empty for the common case where a pkgbase declares all its provides
    /// at the top level.
    pub provides: Vec<String>,
}

/// One pkgbase row. Split-package pkgnames are all listed in `pkgnames`.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Default)]
pub struct IndexEntry {
    /// Pkgbase (also the branch name on the mirror).
    pub pkgbase: PkgBase,
    /// All pkgnames produced by this pkgbase, with their pkgname-scoped
    /// metadata. Single entry for non-split pkgs (where `pkgbase == name`).
    pub pkgnames: Vec<Pkgname>,
    /// `pkgver` field.
    pub pkgver: String,
    /// `pkgrel` field.
    pub pkgrel: String,
    /// Optional `epoch` field (often unset).
    pub epoch: Option<String>,
    /// One-line description (`pkgdesc`).
    pub pkgdesc: Option<String>,
    /// Runtime dependencies.
    pub depends: Vec<String>,
    /// Build-time dependencies.
    pub makedepends: Vec<String>,
    /// Test/check dependencies.
    pub checkdepends: Vec<String>,
    /// Optional runtime dependencies (with `: reason` suffixes preserved).
    pub optdepends: Vec<String>,
    /// Pkgbase-level `provides` (declared *before* any `pkgname = …` line in
    /// `.SRCINFO` — they apply to every pkgname). Pkgname-scoped provides
    /// live on [`Pkgname::provides`] inside `pkgnames`. Callers that don't
    /// care about attribution should use [`IndexEntry::all_provides`] to
    /// iterate both buckets together.
    pub provides: Vec<String>,
    /// `conflicts` declarations.
    pub conflicts: Vec<String>,
    /// `replaces` declarations.
    pub replaces: Vec<String>,
    /// Supported `arch` list.
    pub arch: Vec<String>,
    /// Commit OID of the branch tip that produced this entry.
    pub commit_oid: [u8; 20],
    /// Blob OID of the `.SRCINFO` file inside that commit's tree.
    pub srcinfo_blob_oid: [u8; 20],
}

/// Top-level archive: header metadata + entries sorted by `pkgbase`.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Default)]
pub struct IndexFile {
    /// Format version, bumped on incompatible schema changes.
    pub format_version: u32,
    /// HEAD of the mirror at the time this index was written.
    pub mirror_head_oid: [u8; 20],
    /// Unix timestamp of last index write.
    pub built_at_unix: u64,
    /// Entries, sorted by pkgbase for stable diffs.
    pub entries: Vec<IndexEntry>,
}

impl IndexEntry {
    /// Iterate every `provides` declared anywhere in this pkgbase —
    /// pkgbase-level first, then each pkgname's scoped provides. Order is
    /// not significant; callers that need attribution should walk
    /// `pkgnames` directly.
    pub fn all_provides(&self) -> impl Iterator<Item = &str> {
        self.provides.iter().map(String::as_str).chain(
            self.pkgnames
                .iter()
                .flat_map(|p| p.provides.iter().map(String::as_str)),
        )
    }

    /// Pacman-style `[epoch:]pkgver-pkgrel` combined version. Returned as
    /// the typed [`Version`] so callers get vercmp on `<` / `==` by default.
    /// Empty `epoch` is treated the same as no epoch — matches what the
    /// raw `version_string` helper used to produce.
    pub fn version(&self) -> Version {
        let epoch = self
            .epoch
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|s| format!("{s}:"))
            .unwrap_or_default();
        Version::new(format!("{epoch}{}-{}", self.pkgver, self.pkgrel))
    }
}

impl IndexFile {
    /// Current format version constant. Bumped to **3** when `pkgbase` and
    /// `Pkgname.name` switched from `String` to the typed `PkgBase` / `PkgName`
    /// newtypes. rkyv archives are distinct per Rust type even when the
    /// underlying bytes match, so loading a v2 file with v3 types would
    /// silently mis-shape the deserialized struct without the version
    /// gate. v1/v2 archives must be rebuilt via `gitaur -Sy`.
    pub const FORMAT_VERSION: u32 = 3;

    /// Empty in-memory index. Used when no on-disk file exists yet.
    pub fn empty() -> Self {
        Self {
            format_version: Self::FORMAT_VERSION,
            mirror_head_oid: [0u8; 20],
            built_at_unix: 0,
            entries: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_with_epoch() {
        let e = IndexEntry {
            pkgver: "1.0".into(),
            pkgrel: "2".into(),
            epoch: Some("3".into()),
            ..Default::default()
        };
        assert_eq!(e.version(), "3:1.0-2");
    }

    #[test]
    fn version_without_epoch() {
        let e = IndexEntry {
            pkgver: "1.0".into(),
            pkgrel: "2".into(),
            ..Default::default()
        };
        assert_eq!(e.version(), "1.0-2");
    }

    #[test]
    fn version_treats_empty_epoch_as_none() {
        let e = IndexEntry {
            pkgver: "1.0".into(),
            pkgrel: "2".into(),
            epoch: Some(String::new()),
            ..Default::default()
        };
        assert_eq!(e.version(), "1.0-2");
    }
}
