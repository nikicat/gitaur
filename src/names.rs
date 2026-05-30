//! Typed wrappers for [`PkgName`] and [`PkgBase`].
//!
//! The two name-shaped strings gitaur conflates at its peril: `PkgName` is
//! a pacman pkgname (what `pacman -Q` reports), `PkgBase` is an AUR pkgbase
//! (a branch name on the mirror, the makepkg unit).
//!
//! For non-split pkgs the two lexical strings are identical (`foo`'s
//! pkgbase is also `foo`), which is exactly when accidental cross-passing
//! is silent at runtime. Newtype wrappers make the distinction load-bearing
//! at compile time: a `fn take_pkgbase(&PkgBase)` can't be handed a
//! `&PkgName`.
//!
//! Both wrappers are transparent over `String`. The trait surface is
//! deliberately narrow — anything wider would re-introduce the
//! "everything is just a string" sloppiness the newtypes exist to prevent:
//!
//!   * **No `Deref<Target=str>`, no `AsRef<str>`.** Code that needs an
//!     intrinsic-of-`str` operation (`ends_with`, regex matching, etc.)
//!     either gets a dedicated method on the newtype (e.g.
//!     [`PkgBase::is_vcs`]) or calls [`Self::as_str`] explicitly. Both
//!     paths put the conversion in plain sight at review time.
//!   * **`Borrow<str>` is kept** — that's the one mechanical interop
//!     needed so `HashMap<PkgBase, V>::get("foo")` works without
//!     allocating a temporary. Without it, every lookup would noisy up
//!     into `map.get(&PkgBase::from(s))`.
//!   * **`From<&str>` / `From<String>` are kept** for ergonomic
//!     construction — parsers, fixtures, and explicit conversions at
//!     untyped boundaries (CLI argv, srcinfo input) all need them.
//!   * **`PartialEq<&str>` / `PartialEq<String>` are kept** for tests and
//!     for literal-equality assertions (`assert_eq!(e.pkgbase, "foo")`).
//!
//! Pacman version strings live in [`crate::version::Version`] / `Ver` —
//! the type rules there are intentionally different (`Deref<Target=Ver>`,
//! `<` and `==` are vercmp), so the module is separate.
//!
//! Boundaries that deliberately stay `String` / `&str`:
//!   * The user's typed CLI / picker target — could be any kind of name
//!     plus an optional version constraint; only classifiable after
//!     `expand_pkgbase_targets` runs.
//!   * The alpm crate boundary — `alpm::Alpm` consumes plain `&str`.
//!   * The alpm crate boundary already noted above; pkgbase-level
//!     `provides=`/`conflicts=`/`replaces=` declarations in `IndexEntry`
//!     are typed as `Vec<PkgTarget>` (versioned dep specs), with their
//!     bare name retrievable via [`PkgTarget::bare`].

use regex::Regex;
use rkyv::{Archive, Deserialize, Serialize};
use std::borrow::Borrow;
use std::fmt;
use std::path::Path;

/// One pacman pkgname (the entity `pacman -Q` reports, the unit of a
/// localdb entry). For split AUR pkgbases there's more than one per
/// pkgbase; for non-split pkgs `PkgName == PkgBase` lexically.
#[derive(
    Archive, Serialize, Deserialize, Debug, Clone, Default, Hash, PartialEq, Eq, PartialOrd, Ord,
)]
#[rkyv(compare(PartialEq, PartialOrd))]
pub struct PkgName(String);

/// One AUR pkgbase — the branch name on the mirror, the unit `makepkg`
/// builds. Always a single pkgbase per AUR repo branch; produces one or
/// more pkgnames.
#[derive(
    Archive, Serialize, Deserialize, Debug, Clone, Default, Hash, PartialEq, Eq, PartialOrd, Ord,
)]
#[rkyv(compare(PartialEq, PartialOrd))]
pub struct PkgBase(String);

/// An unclassified dep-shaped name — pkgname, pkgbase, virtual, or
/// versioned (`foo>=1.2`).
///
/// Two distinct provenances feed this type:
///
/// * **User-typed install targets** at the CLI / picker boundary, where
///   [`crate::resolver::pkgbase_expand`] later resolves each into a
///   `PkgBase` (rewritten) or keeps it as a String passthrough.
/// * **AUR-declared dep specs** in `IndexEntry.{provides,conflicts,
///   replaces}` and `Pkgname.provides`, parsed once from `.SRCINFO` and
///   archived in the rkyv index. These can carry version constraints, so
///   they can't reduce to `PkgName` / `VirtualName` (which are bare-name
///   types).
///
/// The point of the type is to make "I haven't classified this yet" a
/// compile-time fact: a function taking `&PkgTarget` cannot be handed a
/// `&PkgName` or `&PkgBase`, so the heterogeneous CLI / index-archive
/// boundaries can't be mistaken for a classified one.
#[derive(
    Archive, Serialize, Deserialize, Debug, Clone, Default, Hash, PartialEq, Eq, PartialOrd, Ord,
)]
#[rkyv(compare(PartialEq, PartialOrd))]
pub struct PkgTarget(String);

/// A virtual name declared in an AUR pkg's `provides=` array — **not** a pkgname.
///
/// It's a "this package satisfies the name X" claim, and multiple pkgs can
/// declare the same virtual. Distinct from [`PkgName`] at the type level
/// because their semantic origins differ even when they share lexical
/// shape: a `dotnet-runtime-7.0` `PkgName` is the thing pacman has
/// installed under that name; a `dotnet-runtime-7.0` `VirtualName` is what
/// AUR pkg `dotnet-core-7.0-bin` claims to
/// satisfy. The cross-domain bridge between the two — "is this `PkgName`
/// lexically the same as some pkg's `VirtualName`?" — lives in
/// [`crate::index::secondary::Secondary::classify_foreign`].
#[derive(
    Archive, Serialize, Deserialize, Debug, Clone, Default, Hash, PartialEq, Eq, PartialOrd, Ord,
)]
#[rkyv(compare(PartialEq, PartialOrd))]
pub struct VirtualName(String);

macro_rules! impl_name_wrapper {
    ($ty:ident) => {
        impl $ty {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }
            pub fn into_inner(self) -> String {
                self.0
            }
            pub const fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
            pub const fn len(&self) -> usize {
                self.0.len()
            }
            /// Match the wrapped name against a compiled regex. Routing
            /// this through a typed method (instead of exposing `&str`)
            /// keeps regex matching from looking like a generic "treat
            /// the name as a string" — the call reads as a domain
            /// operation, not a downgrade.
            pub fn matches_regex(&self, r: &Regex) -> bool {
                r.is_match(&self.0)
            }
            /// Prefix test as a domain operation — keeps cluster/family
            /// checks (e.g. `python38-*`) from reaching into the inner
            /// string just to call `str::starts_with`.
            pub fn starts_with(&self, prefix: &str) -> bool {
                self.0.starts_with(prefix)
            }
        }

        impl From<String> for $ty {
            fn from(s: String) -> Self {
                Self(s)
            }
        }

        impl From<&str> for $ty {
            fn from(s: &str) -> Self {
                Self(s.to_owned())
            }
        }

        impl From<&String> for $ty {
            fn from(s: &String) -> Self {
                Self(s.clone())
            }
        }

        // `Borrow<str>` is kept (and `Borrow<str>` only — no `AsRef<str>`,
        // no `Deref<Target=str>`) so `HashMap<PkgBase, V>::get("foo")`
        // works without a temporary newtype allocation. That's the one
        // mechanical interop we need; everything else is intentional.
        impl Borrow<str> for $ty {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        // `AsRef<Path>` lets `PathBuf::join(&pkgbase)` work without routing
        // through `Display` / `to_string()` — a pkgname or pkgbase IS a
        // legitimate path component, so the cast is genuine and not a
        // "smuggle the string out" downgrade. Same logic as `String:
        // AsRef<Path>` in std.
        impl AsRef<Path> for $ty {
            fn as_ref(&self) -> &Path {
                Path::new(&self.0)
            }
        }

        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.pad(&self.0)
            }
        }

        // `==` against a plain string so callers can write
        // `pkgbase == "foo"` without an explicit ctor; `PartialEq<&str>`
        // also covers `&"foo"` for HashMap-key comparisons in some patterns.
        impl PartialEq<str> for $ty {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }
        impl PartialEq<&str> for $ty {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }
        impl PartialEq<String> for $ty {
            fn eq(&self, other: &String) -> bool {
                &self.0 == other
            }
        }
    };
}

impl_name_wrapper!(PkgName);
impl_name_wrapper!(PkgBase);
impl_name_wrapper!(PkgTarget);
impl_name_wrapper!(VirtualName);

// Cross-type conversions, intentionally only in the "narrowing → widening"
// direction. A classified `PkgBase` or `PkgName` can be re-presented as an
// unclassified `PkgTarget` (e.g. when `expand_pkgbase_targets` rewrites a
// user target to the pkgbase string for the resolver to chew through). The
// reverse — `PkgTarget` → `PkgBase` / `PkgName` — is NOT a `From` impl: it
// requires actual lookup against the AUR index, and forcing callers to
// route through the classifier keeps unsound implicit casts off the table.
impl From<PkgBase> for PkgTarget {
    fn from(b: PkgBase) -> Self {
        Self(b.0)
    }
}

impl From<&PkgBase> for PkgTarget {
    fn from(b: &PkgBase) -> Self {
        Self(b.0.clone())
    }
}

impl From<PkgName> for PkgTarget {
    fn from(n: PkgName) -> Self {
        Self(n.0)
    }
}

impl From<&PkgName> for PkgTarget {
    fn from(n: &PkgName) -> Self {
        Self(n.0.clone())
    }
}

impl PkgTarget {
    /// Strip any pacman dep-style version constraint (`>=`, `<=`, `=`,
    /// `<`, `>`) plus the version expression after it. Returns the bare
    /// name suitable for lookup against `Secondary` / `PacmanIndex`.
    /// Mirrors [`crate::index::secondary::strip_version_constraint`]; kept
    /// here so `PkgTarget` owns its own normalisation rather than callers
    /// reaching into a different module.
    pub fn bare(&self) -> &str {
        for op in [">=", "<=", "==", ">", "<", "="] {
            if let Some(idx) = self.0.find(op) {
                return self.0[..idx].trim();
            }
        }
        self.0.trim()
    }

    /// Cross-identity bridge: "does this dep-spec's bare name name the
    /// given identifier?" Encapsulates the single `Borrow<str>` step
    /// between the typed dep-spec (`provides=`/`conflicts=`/`replaces=` in
    /// `IndexEntry`) and any typed name (`PkgName`, `PkgBase`, …) so call
    /// sites read as a domain operation, not a string compare. Uses the
    /// same `Borrow<str>` trait that `HashMap<PkgBase, V>::get("foo")`
    /// already relies on as the canonical "names a pkg identifier" cast.
    pub fn refers_to<N>(&self, name: &N) -> bool
    where
        N: ?Sized + Borrow<str>,
    {
        self.bare() == name.borrow()
    }
}

impl PkgName {
    /// True for makepkg-generated `-debug` split packages (produced by
    /// `OPTIONS=(debug)` in `makepkg.conf`). They never appear in the AUR
    /// index on their own, so the `-Syu` upgrade-query path suppresses
    /// "foreign pkg not in AUR index" warnings for them.
    pub fn is_makepkg_debug_split(&self) -> bool {
        self.0.ends_with("-debug")
    }
}

/// Membership checks across the typed name domains.
///
/// Each method names the semantic claim ("does this set of user-typed
/// targets contain this pkgname?") and encapsulates the single
/// `Borrow<str>` cast it requires — keeps the cross-domain string-match
/// away from generic call sites.
pub trait PkgTargetSetExt {
    /// True iff some [`PkgTarget`] in `self` is lexically the same string
    /// as `pkgname`. The cross-domain string-match claim: "did the user
    /// type this pkgname (or anything lexically equal to it) as a
    /// target?". Used by `install_stratum` to flip built pkgs from
    /// `--asdeps` to Explicit when they appear on the user's command line.
    fn contains_pkgname(&self, pkgname: &PkgName) -> bool;
}

impl<S: std::hash::BuildHasher> PkgTargetSetExt for std::collections::HashSet<PkgTarget, S> {
    fn contains_pkgname(&self, pkgname: &PkgName) -> bool {
        // The single Borrow<str> probe lives here, documenting the
        // cross-identity claim once.
        self.contains(<PkgName as Borrow<str>>::borrow(pkgname))
    }
}

impl PkgBase {
    /// True for VCS-tracked pkgbases (`-git`, `-svn`, `-hg`, `-bzr`). Their
    /// static `pkgver` field is meaningless (overridden by the `pkgver()`
    /// function at build time), so the upgrade-query path and the review
    /// header treat them as always-outdated when `--devel` is on.
    pub fn is_vcs(&self) -> bool {
        let s = &self.0;
        s.ends_with("-git") || s.ends_with("-svn") || s.ends_with("-hg") || s.ends_with("-bzr")
    }

    /// True when this pkgbase's string is identical to the given pkgname —
    /// the canonical (non-split) case where `pkgbase == pkgname`. Used by
    /// review/header logic to decide whether a split-pkg annotation
    /// (`(pkgname: foo-cli)`) adds information or is redundant. The two
    /// types deliberately don't `PartialEq`-cross; this method is the
    /// dedicated place where their lexical equality is meaningful.
    pub fn matches_pkgname(&self, pkgname: &PkgName) -> bool {
        self.0 == pkgname.0
    }

    /// Construct the canonical pkgname for this pkgbase, when the data
    /// model already guarantees they share one string. The narrow cases:
    ///
    ///   * `.SRCINFO` parsing where no explicit `pkgname = …` was declared
    ///     — Arch semantics say pkgname defaults to pkgbase.
    ///   * Test fixtures that construct an entry canonical by design.
    ///
    /// **Not** a general `PkgBase` → `PkgName` conversion: a split pkgbase
    /// has multiple pkgnames, none of which necessarily equal the pkgbase
    /// string. Calling this on a split-pkg `PkgBase` produces a `PkgName`
    /// that the rest of the system has no reason to recognise.
    pub fn canonical_pkgname(&self) -> PkgName {
        PkgName(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// `Borrow<str>` is the load-bearing impl for HashMap-key
    /// interoperability — `map.get("foo")` must work on `HashMap<PkgBase, V>`
    /// without constructing a temporary `PkgBase`.
    #[test]
    fn hashmap_key_lookup_works_with_str() {
        let mut m: HashMap<PkgBase, u32> = HashMap::new();
        m.insert(PkgBase::from("bisq"), 1);
        assert_eq!(m.get("bisq"), Some(&1));
    }

    // The "PkgName and PkgBase are not cross-comparable" guarantee is a
    // compile-time invariant exercised by every fn signature in the tree
    // (anything taking `&PkgBase` rejects `&PkgName` at the type checker).
    // A runtime test of lexical equality would add nothing — it'd only
    // verify `String::eq`, which Rust already guarantees.

    #[test]
    fn display_returns_raw_string() {
        assert_eq!(
            format!("{}", PkgBase::from("dotnet-core-7.0-bin")),
            "dotnet-core-7.0-bin"
        );
    }

    /// The upgrade table aligns the name column with `{name:<W$}`. `Display`
    /// must go through `Formatter::pad` (not `write_str`, which drops
    /// width/fill/align) or the column collapses to natural width.
    #[test]
    fn display_respects_width_and_alignment() {
        assert_eq!(format!("{:<8}", PkgName::from("foo")), "foo     ");
        assert_eq!(format!("{:<8}", PkgBase::from("foo")), "foo     ");
        assert_eq!(format!("{:>8}", PkgName::from("foo")), "     foo");
        assert_eq!(format!("{:*^7}", PkgName::from("foo")), "**foo**");
    }

    #[test]
    fn from_str_and_string_both_work() {
        let a = PkgName::from("foo");
        let b = PkgName::from(String::from("foo"));
        let c: PkgName = "foo".into();
        assert_eq!(a, b);
        assert_eq!(a, c);
    }

    /// `is_vcs()` recognises the four AUR-conventional VCS suffixes and
    /// nothing else — `git-lfs` is a real pkg, not a VCS shim.
    #[test]
    fn pkgbase_is_vcs_detects_vcs_suffixes() {
        assert!(PkgBase::from("neovim-git").is_vcs());
        assert!(PkgBase::from("foo-svn").is_vcs());
        assert!(PkgBase::from("bar-hg").is_vcs());
        assert!(PkgBase::from("baz-bzr").is_vcs());
        assert!(!PkgBase::from("neovim").is_vcs());
        assert!(!PkgBase::from("git-lfs").is_vcs());
    }
}
