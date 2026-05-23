//! Typed pacman version strings (`[epoch:]pkgver-pkgrel`) with `vercmp`
//! semantics baked in.
//!
//! Two types in the `String`/`str` shape:
//!   * [`Version`] — owned, stored in struct fields (`PkgUpgrade`,
//!     `PacmanIndex.installed`, `IndexEntry::version()` return).
//!   * [`Ver`] — `#[repr(transparent)]` borrowed view, the natural
//!     parameter type for functions taking a version (`fn is_outdated(&Ver,
//!     &Ver)`). `Deref<Target=Ver>` converts `&Version → &Ver`, mirroring
//!     `&String → &str`.
//!
//! **`<` and `==` mean vercmp, not lexical.**
//! `Version::from("1.10") > Version::from("1.9")` even though `"1.10"` <
//! `"1.9"` as strings. This is the opposite design choice from
//! [`crate::names`]: for names, `Deref<Target=str>` would invite
//! string-shaped operations on what should be opaque identities, so we
//! deliberately banned it. For versions, the natural operation IS
//! vercmp — so making `<` / `==` mean vercmp via `Deref<Target=Ver>` puts
//! the right semantics in the default code path.
//!
//! **No `Hash` / `Eq` / `Ord` derives.** vercmp comparison is a total
//! function but `PartialEq` via vercmp doesn't necessarily round-trip with
//! lexical-byte equality (vercmp normalises). `Hash` over the inner bytes
//! plus `Eq` via vercmp would violate the `Hash ↔ Eq` invariant. The use
//! cases for `Version` are all "value in a struct field" or "compare two
//! versions" — never a `HashMap` key — so we just don't expose the broken
//! traits.
//!
//! **No `AsRef<str>` / no `as_str()`-as-conversion.** Same reasoning as
//! the name types: code that wants to print or join version text uses
//! `Display`; code that wants the raw inner string for serialization uses
//! `Ver::as_str()` explicitly. `into_inner()` exists on `Version` for the
//! same reason `PkgBase::into_inner` does — at boundaries that genuinely
//! need a `String` (rkyv archived fields, CLI argv).
//!
//! **alpm boundary.** `alpm::vercmp` is what we call. Re-exported via
//! [`vercmp`] for the rare procedural site that prefers a free function
//! to method dispatch.

use rkyv::{Archive, Deserialize, Serialize};
use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt;
use std::ops::Deref;

/// Owned pacman version string. Storage-form of [`Ver`].
///
/// Stored in `IndexEntry`'s combined version (via `version()`), `PkgUpgrade`,
/// and `PacmanIndex.installed` / `sync_versions`. Construct with `From<&str>`
/// or `Version::new`; compare with `<` / `==` (vercmp via `Deref<Target=Ver>`).
#[derive(Archive, Serialize, Deserialize, Debug, Clone, Default)]
pub struct Version(pub String);

/// Borrowed pacman version. `#[repr(transparent)]` over `str`, so a `&Ver`
/// is layout-identical to a `&str` — the only mechanism that constructs
/// one is [`Ver::new`], which is a zero-cost cast.
///
/// Functions that take "a version" should accept `&Ver`; callers pass a
/// `&Version` (auto-derefs) or `Ver::new("1.0-1")` for literals.
#[repr(transparent)]
#[derive(Debug)]
pub struct Ver(str);

impl Version {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Surrender the typed wrapper at a `String`-only boundary (rkyv
    /// archived fields, CLI argv concatenation). Mirrors `PkgBase::into_inner`.
    pub fn into_inner(self) -> String {
        self.0
    }

    /// Borrow as `&Ver`. `Deref` provides this implicitly; the explicit
    /// method is useful in struct-method bodies where deref-coercion
    /// doesn't reach.
    pub fn as_ver(&self) -> &Ver {
        Ver::new(&self.0)
    }

    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Ver {
    /// Borrow a `&str` as `&Ver`. Zero-cost; `#[repr(transparent)]` makes
    /// the layouts identical.
    ///
    /// `#[allow(unsafe_code)]` is scoped to this one function — the crate
    /// otherwise denies unsafe. The cast is exactly the same pattern alpm's
    /// own `Ver` uses (`alpm-4/src/version.rs`) and is the standard safe
    /// API for a `#[repr(transparent)]` newtype around an unsized type:
    /// without it, `&Ver` would be unconstructable.
    #[allow(unsafe_code)]
    pub const fn new(s: &str) -> &Self {
        // SAFETY: `Ver` is `#[repr(transparent)]` over `str`, so `*const str`
        // and `*const Ver` point to the same bytes with the same metadata.
        // No `Drop`, no fields beyond the `str`, no inner mutability.
        unsafe { &*(std::ptr::from_ref::<str>(s) as *const Self) }
    }

    /// Raw underlying text. Use for serialisation / Display-equivalent
    /// rendering; comparisons should go through `==` / `<` to invoke vercmp.
    pub const fn as_str(&self) -> &str {
        &self.0
    }

    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Byte length of the underlying text. Used by the upgrade table for
    /// column-width math; not a domain operation. Same justification as
    /// `PkgBase::len` — it's a pure UI concern.
    pub const fn len(&self) -> usize {
        self.0.len()
    }

    /// True iff `self` is strictly older than `newer` per vercmp. The
    /// common "needs upgrade?" question; equivalent to `self < newer` but
    /// reads more naturally at call sites that don't immediately suggest
    /// a comparison.
    pub fn is_outdated(&self, newer: &Self) -> bool {
        self < newer
    }

    /// Strip `<self>-` from the start of `filename`, returning the remainder
    /// (the `<arch>.pkg.tar.{zst,xz}` tail). Encapsulates the pacman
    /// filename grammar — `<pkgname>-<version>-<arch>.pkg.tar.{zst,xz}` —
    /// so the raw `as_str()` byte-match stays here instead of leaking into
    /// the build's idempotency check.
    pub fn strip_filename_segment<'a>(&self, filename: &'a str) -> Option<&'a str> {
        filename
            .strip_prefix(self.as_str())
            .and_then(|r| r.strip_prefix('-'))
    }
}

impl Deref for Version {
    type Target = Ver;
    fn deref(&self) -> &Ver {
        self.as_ver()
    }
}

impl Borrow<Ver> for Version {
    fn borrow(&self) -> &Ver {
        self.as_ver()
    }
}

impl ToOwned for Ver {
    type Owned = Version;
    fn to_owned(&self) -> Version {
        Version(self.0.to_owned())
    }
}

impl From<String> for Version {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Version {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<&String> for Version {
    fn from(s: &String) -> Self {
        Self(s.clone())
    }
}

impl From<&Ver> for Version {
    fn from(v: &Ver) -> Self {
        Self(v.0.to_owned())
    }
}

/// Boundary conversion at the alpm seam. `ipkg.version()` returns
/// `&alpm::Ver`; this `From` impl is what produces a typed gitaur `Version`
/// without going through `Display::to_string`. Mirrors the
/// `PkgName::new(ipkg.name())` boundary in `PacmanIndex::build`.
impl From<&alpm::Ver> for Version {
    fn from(v: &alpm::Ver) -> Self {
        Self(v.as_str().to_owned())
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.0)
    }
}

impl fmt::Display for Ver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Vercmp-based comparison. Implemented on `Ver`; `Version` inherits via
// `Deref<Target=Ver>` for `<` and `==` against another `Version`, plus an
// explicit `PartialEq<Version>` impl since auto-deref doesn't kick in for
// trait dispatch on the RHS.

impl PartialEq for Ver {
    fn eq(&self, other: &Self) -> bool {
        vercmp(self, other) == Ordering::Equal
    }
}

impl PartialOrd for Ver {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(vercmp(self, other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.as_ver() == other.as_ver()
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.as_ver().partial_cmp(other.as_ver())
    }
}

// Cross-type comparisons so `&Version` compares directly against `&Ver`
// (e.g. when calling a function that returns `&Ver` and comparing the
// result to a stored `Version` without `.as_ver()` everywhere).
impl PartialEq<Ver> for Version {
    fn eq(&self, other: &Ver) -> bool {
        self.as_ver() == other
    }
}

// `Version == &Ver` (and `&Version == &Ver`) — the shape that comes up
// when comparing a stored owned version against the borrowed result of a
// getter like `installed_version() -> Option<&Ver>`.
impl PartialEq<&Ver> for Version {
    fn eq(&self, other: &&Ver) -> bool {
        self.as_ver() == *other
    }
}

impl PartialEq<Version> for Ver {
    fn eq(&self, other: &Version) -> bool {
        self == other.as_ver()
    }
}

impl PartialOrd<Ver> for Version {
    fn partial_cmp(&self, other: &Ver) -> Option<Ordering> {
        self.as_ver().partial_cmp(other)
    }
}

impl PartialOrd<Version> for Ver {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        self.partial_cmp(other.as_ver())
    }
}

// String-literal equality on `Version` for assertion ergonomics — same
// rationale as `PkgBase: PartialEq<&str>` in `names`. Note this is *vercmp*
// equality (`Version::from("1.0") == "1.0-1"` is false because vercmp sees
// the missing pkgrel), not byte equality.
impl PartialEq<&str> for Version {
    fn eq(&self, other: &&str) -> bool {
        self.as_ver() == Ver::new(other)
    }
}

impl PartialEq<str> for Version {
    fn eq(&self, other: &str) -> bool {
        self.as_ver() == Ver::new(other)
    }
}

impl PartialEq<&str> for Ver {
    fn eq(&self, other: &&str) -> bool {
        self == Self::new(other)
    }
}

impl PartialEq<str> for Ver {
    fn eq(&self, other: &str) -> bool {
        self == Self::new(other)
    }
}

// ---------------------------------------------------------------------------

/// Free-function vercmp for the rare procedural callsite that prefers it
/// over `Ver`'s `<` / `==` (e.g. inside a sort closure on raw `&str`).
/// Identical semantics to `<Ver as PartialOrd>::partial_cmp`.
pub fn vercmp(a: &Ver, b: &Ver) -> Ordering {
    alpm::vercmp(a.as_str(), b.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_ordering_is_vercmp_not_lexical() {
        // "1.10" lexically < "1.9" but vercmp puts 1.10 *after* 1.9.
        let a = Version::from("1.10");
        let b = Version::from("1.9");
        assert!(a > b, "lexical: {a} < {b}; vercmp: {a} > {b}");
    }

    #[test]
    fn equality_is_vercmp_equality() {
        assert_eq!(Version::from("1.0-1"), Version::from("1.0-1"));
        assert_ne!(Version::from("1.0-1"), Version::from("1.0-2"));
    }

    #[test]
    fn epoch_dominates_pkgver() {
        // `1:1.0` is newer than `999.0` regardless of pkgver — epoch wins.
        let with_epoch = Version::from("1:1.0");
        let without = Version::from("999.0");
        assert!(with_epoch > without);
    }

    #[test]
    fn pkgrel_breaks_ties() {
        let a = Version::from("1.0-1");
        let b = Version::from("1.0-2");
        assert!(a < b);
        assert!(a.is_outdated(&b));
        assert!(!b.is_outdated(&a));
    }

    #[test]
    fn deref_lets_version_act_as_ver() {
        // Coerce to &Ver via Deref to pass to a Ver-taking function.
        fn ver_len(v: &Ver) -> usize {
            v.as_str().len()
        }
        let v = Version::from("1.0-1");
        assert_eq!(ver_len(&v), 5);
    }

    #[test]
    fn cross_type_comparisons_compile_and_match() {
        let owned = Version::from("1.0-1");
        let borrowed = Ver::new("1.0-1");
        assert_eq!(&owned, borrowed);
        assert_eq!(borrowed, &owned);
        let newer = Version::from("1.1-1");
        assert!(*borrowed < newer);
    }

    #[test]
    fn string_literal_equality_uses_vercmp() {
        let v = Version::from("1.0");
        // "1.0" vs "1.0.0" — vercmp considers these equal (trailing zero
        // segments are ignored). Document the surprise.
        assert_eq!(v, "1.0");
        // Bytes-distinct but vercmp-equal:
        assert_eq!(Version::from("1.0"), "1.0");
        // Distinct under vercmp:
        assert_ne!(Version::from("1.0-1"), "1.0-2");
    }

    #[test]
    fn ver_new_is_zero_cost_cast() {
        let s = "1.0-1";
        let v = Ver::new(s);
        // Same address — proves #[repr(transparent)] over str.
        assert_eq!(v.as_str().as_ptr(), s.as_ptr());
        assert_eq!(v.as_str().len(), s.len());
    }

    #[test]
    fn to_owned_round_trips() {
        let borrowed = Ver::new("1.0-1");
        let owned: Version = borrowed.to_owned();
        assert_eq!(&owned, borrowed);
    }

    #[test]
    fn display_emits_raw_text() {
        // Bytes-distinct but vercmp-equal: Display preserves the *raw*
        // shape (no normalisation), so the user sees what they typed.
        assert_eq!(format!("{}", Version::from("1.0")), "1.0");
        assert_eq!(format!("{}", Ver::new("1:1.0-1")), "1:1.0-1");
    }

    #[test]
    fn is_outdated_helper() {
        let installed = Version::from("1.0-1");
        let available = Version::from("1.0-2");
        assert!(installed.is_outdated(&available));
        assert!(!available.is_outdated(&installed));
        // Identical → not outdated (vercmp == Equal, < returns false).
        assert!(!installed.is_outdated(&installed));
    }

    #[test]
    fn display_respects_width_and_alignment() {
        // The upgrade table relies on `{ver:<W$}` padding to align the old/new
        // version columns. `Display` must route through `Formatter::pad` (not
        // `write_str`, which silently drops width/fill/align flags) — otherwise
        // every row collapses to its natural length and the columns mis-align.
        assert_eq!(format!("{:<8}", Version::from("1.0-1")), "1.0-1   ");
        assert_eq!(format!("{:<8}", Ver::new("1.0-1")), "1.0-1   ");
        assert_eq!(format!("{:>8}", Version::from("1.0-1")), "   1.0-1");
        assert_eq!(format!("{:*^9}", Ver::new("1.0-1")), "**1.0-1**");
    }
}
