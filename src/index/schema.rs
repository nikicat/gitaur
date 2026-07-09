//! On-disk index schema. Persisted via `rkyv 0.8` zero-copy archive.

use crate::names::{PkgBase, PkgName, PkgTarget};
use crate::version::Version;
use rkyv::{Archive, Deserialize, Serialize};

/// One pkgname's metadata inside a pkgbase.
///
/// Split-package PKGBUILDs override some fields per-pkgname; pkgbase-level
/// fields on [`IndexEntry`] apply to every pkgname implicitly and are not
/// duplicated here.
///
/// Carries pkgname-scoped `provides` (needed by the resolver to disambiguate
/// split packages — e.g. yay-style `-S bisq` matching `bisq-desktop`'s
/// `provides=bisq`, not the other two siblings) and `pkgdesc` (split packages
/// frequently describe each member separately, with no pkgbase-level desc —
/// see [`IndexEntry::display_desc`]). Pkgname-scoped `depends`, `conflicts`,
/// `replaces` etc. can be added the same way if a future feature needs them.
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct Pkgname {
    /// The pkgname itself.
    pub name: PkgName,
    /// `provides = …` declared inside this pkgname's section in `.SRCINFO`.
    /// Empty for the common case where a pkgbase declares all its provides
    /// at the top level.
    pub provides: Vec<PkgTarget>,
    /// `pkgdesc = …` declared inside this pkgname's section in `.SRCINFO`.
    /// `None` when the description is declared once at the pkgbase level
    /// (the common non-split case) — that value lives on
    /// [`IndexEntry::pkgdesc`] instead.
    pub pkgdesc: Option<String>,
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
    /// Pkgbase-level one-line description (`pkgdesc` declared *before* any
    /// `pkgname = …` line). `None` for split packages that describe each
    /// member individually — those descriptions live on [`Pkgname::pkgdesc`].
    /// Use [`IndexEntry::display_desc`] for the headline shown in the picker
    /// and `-Ss`, which falls back to the per-pkgname value.
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
    pub provides: Vec<PkgTarget>,
    /// `conflicts` declarations.
    pub conflicts: Vec<PkgTarget>,
    /// `replaces` declarations.
    pub replaces: Vec<PkgTarget>,
    /// Supported `arch` list.
    pub arch: Vec<String>,
    /// Commit OID of the branch tip that produced this entry.
    pub commit_oid: [u8; 20],
    /// Blob OID of the `.SRCINFO` file inside that commit's tree.
    pub srcinfo_blob_oid: [u8; 20],
    /// Committer timestamp (seconds since the Unix epoch) of the branch tip
    /// that produced this entry. Drives the "freshest first" ordering of the
    /// `aurox <term>` picker — recently-pushed AUR packages float to the top.
    /// `0` for entries built before this field existed or whose commit time
    /// couldn't be read.
    pub commit_time_unix: i64,
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

/// Pass through a description, treating an empty string as absent.
fn nonempty(d: Option<&str>) -> Option<&str> {
    d.filter(|s| !s.is_empty())
}

impl IndexEntry {
    /// Iterate every `provides` declared anywhere in this pkgbase —
    /// pkgbase-level first, then each pkgname's scoped provides. Order is
    /// not significant; callers that need attribution should walk
    /// `pkgnames` directly.
    pub fn all_provides(&self) -> impl Iterator<Item = &PkgTarget> {
        self.provides
            .iter()
            .chain(self.pkgnames.iter().flat_map(|p| p.provides.iter()))
    }

    /// Headline one-line description for the pkgbase row shown in the picker,
    /// `-Ss`, and `-Si`.
    ///
    /// `.SRCINFO` declares `pkgdesc` either once in the pkgbase header (applies
    /// to every pkgname) or inside individual `pkgname = …` sections (split
    /// packages that describe each member separately, with no pkgbase-level
    /// desc). Prefer the pkgbase-level value; otherwise fall back to the
    /// pkgname matching the pkgbase (the canonical member whose name the picker
    /// displays), then to the first pkgname carrying any description. Empty
    /// strings are skipped at every step so a stray `pkgdesc=` doesn't mask a
    /// real description further down.
    pub fn display_desc(&self) -> Option<&str> {
        nonempty(self.pkgdesc.as_deref())
            .or_else(|| {
                self.pkgnames
                    .iter()
                    .find(|p| self.pkgbase.matches_pkgname(&p.name))
                    .and_then(|p| nonempty(p.pkgdesc.as_deref()))
            })
            .or_else(|| {
                self.pkgnames
                    .iter()
                    .find_map(|p| nonempty(p.pkgdesc.as_deref()))
            })
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
    /// Current format version constant. Bumped to **5** when
    /// [`Pkgname::pkgdesc`] was added (per-pkgname descriptions for split
    /// packages that omit a pkgbase-level `pkgdesc`). Was **4** when
    /// [`IndexEntry::commit_time_unix`] was added (the branch-tip committer
    /// timestamp the search picker sorts on). Was **3** when `pkgbase` and
    /// `Pkgname.name` switched from `String` to the typed `PkgBase` / `PkgName`
    /// newtypes. **5 → 6** when `provides` / `conflicts` / `replaces` switched
    /// from `Vec<String>` to `Vec<PkgTarget>` — same bytes, distinct rkyv
    /// archive type, so the version gate forces a rebuild via `aurox -Sy`.
    /// rkyv archives are distinct per Rust type even when the underlying
    /// bytes match, and a new field shifts the layout, so loading an older
    /// file with newer types would silently mis-shape the deserialized
    /// struct without the version gate. Older archives must be rebuilt via
    /// `aurox -Sy`.
    pub const FORMAT_VERSION: u32 = 6;

    /// Empty in-memory index. Used when no on-disk file exists yet.
    pub const fn empty() -> Self {
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

    fn pkg(name: &str, desc: Option<&str>) -> Pkgname {
        Pkgname {
            name: name.into(),
            provides: Vec::new(),
            pkgdesc: desc.map(str::to_owned),
        }
    }

    #[test]
    fn display_desc_prefers_pkgbase_level() {
        let e = IndexEntry {
            pkgbase: "foo".into(),
            pkgdesc: Some("base desc".into()),
            pkgnames: vec![pkg("foo", Some("member desc"))],
            ..Default::default()
        };
        assert_eq!(e.display_desc(), Some("base desc"));
    }

    #[test]
    fn display_desc_falls_back_to_canonical_pkgname() {
        // Split package with no pkgbase-level desc: prefer the member whose
        // name matches the pkgbase (the one the picker shows), not a sibling.
        let e = IndexEntry {
            pkgbase: "systemd-selinux".into(),
            pkgdesc: None,
            pkgnames: vec![
                pkg("systemd-libs-selinux", Some("client libraries")),
                pkg("systemd-selinux", Some("service manager")),
            ],
            ..Default::default()
        };
        assert_eq!(e.display_desc(), Some("service manager"));
    }

    #[test]
    fn display_desc_falls_back_to_first_member_when_no_canonical_match() {
        let e = IndexEntry {
            pkgbase: "foo".into(),
            pkgdesc: None,
            pkgnames: vec![pkg("foo-bin", None), pkg("foo-extras", Some("extras"))],
            ..Default::default()
        };
        assert_eq!(e.display_desc(), Some("extras"));
    }

    #[test]
    fn display_desc_skips_empty_strings() {
        let e = IndexEntry {
            pkgbase: "foo".into(),
            pkgdesc: Some(String::new()),
            pkgnames: vec![pkg("foo", Some("real desc"))],
            ..Default::default()
        };
        assert_eq!(e.display_desc(), Some("real desc"));
    }

    #[test]
    fn display_desc_none_when_nothing_anywhere() {
        let e = IndexEntry {
            pkgbase: "foo".into(),
            pkgdesc: None,
            pkgnames: vec![pkg("foo", None)],
            ..Default::default()
        };
        assert_eq!(e.display_desc(), None);
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
