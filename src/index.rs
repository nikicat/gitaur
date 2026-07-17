//! In-memory index of AUR pkgbases, persisted as a single rkyv-archived file.
//!
//! Loaded via mmap with no per-entry deserialization. Name-lookup hashmaps
//! (`by_name`, `by_provides`, `by_pkgbase`) are built post-load in
//! [`lookup`]; [`AurIndexData`] bundles the two into the one value the rest
//! of the crate consumes.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::mirror;
use crate::names::{PkgBase, PkgName, PkgTarget, RepoName};
use crate::pacman::invoke::REPO_AUR;
use crate::paths;
use crate::runopts;
use crate::ui;
use lookup::{AurClass, Lookup};
use rkyv::rancor::Error as RkyvError;
use std::path::Path;
use tracing::{debug, info, instrument};

pub mod build;
pub mod info;
pub mod lookup;
pub mod schema;
pub mod srcinfo;
pub mod update;

pub use info::cmd_info;
pub use schema::{IndexEntry, IndexFile};

/// Magic prefix of the plain-bytes header written ahead of the rkyv archive.
const HEADER_MAGIC: &[u8; 8] = b"AUROXIDX";

/// Header length: magic + LE u32 [`IndexFile::FORMAT_VERSION`] + 4 reserved
/// bytes. 16 so the archive payload after it keeps the read buffer's
/// allocator alignment (rkyv validates `&bytes[HEADER_LEN..]` in place).
const HEADER_LEN: usize = 16;

/// The plain-bytes file header for `version`. Readable without parsing (let
/// alone validating) the archive behind it, which is the whole point: a
/// schema bump changes the archive layout, so only an out-of-archive version
/// lets [`load`] tell "written under another format version" apart from a
/// genuinely corrupted file.
fn file_header(version: u32) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[..8].copy_from_slice(HEADER_MAGIC);
    h[8..12].copy_from_slice(&version.to_le_bytes());
    h
}

/// Load the on-disk index. Returns an empty index if the file is missing.
///
/// Files this build can't read surface as [`Error::IndexIncompatible`], with
/// the reason split three ways off the plain-bytes header: no
/// [`HEADER_MAGIC`] (written by a pre-v9 aurox), a version mismatch (the
/// normal case right after an aurox upgrade bumps
/// [`IndexFile::FORMAT_VERSION`]), or — header intact but rkyv's validator
/// rejects the archive — genuine corruption.
/// Callers that want the index to "just work" use [`load_or_resync`], which
/// catches that variant and rebuilds in place; this low-level entry point is
/// the one [`mirror::cmd_refresh`] uses, where a failed load must *not*
/// recurse back into a resync.
#[instrument]
pub fn load(path: &Path) -> Result<IndexFile> {
    if !path.exists() {
        debug!("index missing; returning empty");
        return Ok(IndexFile::empty());
    }
    let bytes = std::fs::read(path)?;
    let version = match bytes.get(..HEADER_LEN) {
        Some(h) if h.starts_with(HEADER_MAGIC) => u32::from_le_bytes([h[8], h[9], h[10], h[11]]),
        // No header at all: written before the header existed (≤ v8).
        _ => {
            return Err(Error::IndexIncompatible(format!(
                "format predates v{}",
                IndexFile::FORMAT_VERSION,
            )));
        }
    };
    if version != IndexFile::FORMAT_VERSION {
        return Err(Error::IndexIncompatible(format!(
            "format changed (v{version} → v{})",
            IndexFile::FORMAT_VERSION,
        )));
    }
    let idx: IndexFile =
        rkyv::from_bytes::<IndexFile, RkyvError>(&bytes[HEADER_LEN..]).map_err(|e| {
            // The header version matched, so this is not a schema skew — the
            // archive bytes themselves don't validate. The rancor error
            // carries no detail in release builds ("enable debug assertions
            // and the `alloc` feature…"), so keep it for traces and surface
            // a clean reason to the user.
            debug!(error = %e, "rkyv rejected on-disk index");
            Error::IndexIncompatible("on-disk archive corrupted".into())
        })?;
    info!(entries = idx.entries.len(), "index loaded");
    Ok(idx)
}

/// Load the index, transparently resyncing the database on an incompatible
/// archive.
///
/// This is the common case right after `pacman -Syu` bumps aurox and changes
/// [`IndexFile::FORMAT_VERSION`]. On [`Error::IndexIncompatible`] we print a
/// one-line notice, run a normal
/// `-Sy` refresh (which rebuilds the index from the bare mirror), then retry
/// the load *once* and return its result — a second failure propagates rather
/// than looping. Every other outcome (success, missing file → empty, genuine
/// I/O error) is forwarded untouched, so the happy path is identical to
/// [`load`].
///
/// `--noresync` ([`runopts::noresync`]) opts out: the incompatibility is
/// reported as an error with a `-Sy` hint instead of triggering an implicit
/// network fetch + rebuild.
pub fn load_or_resync(cfg: &Config, path: &Path) -> Result<IndexFile> {
    match load(path) {
        Err(Error::IndexIncompatible(reason)) => {
            if runopts::noresync() {
                return Err(Error::IndexIncompatible(format!(
                    "{reason}; --noresync set, run `aurox -Sy` to rebuild"
                )));
            }
            ui::info(&format!("AUR index {reason}; resyncing database"));
            match mirror::cmd_refresh(
                cfg,
                mirror::RefreshReason::IndexResync,
                mirror::RefreshScope::Everything,
            )? {
                mirror::RefreshOutcome::Refreshed => load(path),
                // The refresh left the mirror alone (AUR disabled, bootstrap
                // declined, or no terminal to ask on), so the incompatible
                // index is still there — report why instead of retrying.
                mirror::RefreshOutcome::AurSkipped(cause) => Err(Error::IndexIncompatible(
                    format!("{reason}; AUR refresh skipped ({cause}) — run `aurox -Sy` to rebuild"),
                )),
            }
        }
        other => other,
    }
}

/// Where the AUR half of aurox stands this run — probed once, consulted only
/// for user-facing wording and the shell's first-launch question.
///
/// Functional code never branches on this: when AUR data is unavailable the
/// loader seam ([`crate::build::AurIndexData::load`]) returns an *empty*
/// index, so search/resolve/install/upgrade all take one uniform path and the
/// AUR simply contributes no rows. Pacman-only mode (`aur = false`)
/// deliberately ignores a leftover `index.bin` from before the switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AurState {
    /// `aur` enabled and an index is on disk: full AUR data.
    Ready,
    /// `aur` enabled but never synced (or pruned): AUR loads empty until a
    /// bootstrap runs.
    NotSetUp,
    /// `aur = false` in config.toml: pacman-only mode by choice.
    Disabled,
}

impl AurState {
    /// Probe config + disk. Cheap (one `exists()`), but callers that need the
    /// answer more than once in a command should probe once and pass it down.
    pub fn probe(cfg: &Config) -> Self {
        if !cfg.aur {
            Self::Disabled
        } else if paths::index_path().exists() {
            Self::Ready
        } else {
            Self::NotSetUp
        }
    }
}

/// The loaded AUR data: the index plus its name-[`Lookup`] maps — the one
/// value that answers "what does the AUR have?" for search, resolve, info,
/// and upgrade scans.
///
/// **[`Self::load`] is the one seam where AUR availability affects data
/// flow**: when the AUR is unavailable — never synced, or `aur = false` — it
/// loads *empty*, so every downstream path runs uniformly and the AUR simply
/// contributes no rows. Wording decisions consult [`AurState`] instead of
/// branching on emptiness.
pub struct AurIndexData {
    idx: IndexFile,
    by: Lookup,
    /// Provenance/display label for rows served from this data — always
    /// [`REPO_AUR`] today; the hook for future non-AUR package repos.
    label: RepoName,
}

impl AurIndexData {
    /// Load the on-disk index (resyncing an incompatible one) and build its
    /// lookup maps; empty when [`AurState::probe`] isn't [`AurState::Ready`].
    pub fn load(cfg: &Config) -> Result<Self> {
        if AurState::probe(cfg) != AurState::Ready {
            return Ok(Self::empty());
        }
        Ok(Self::from_index(load_or_resync(cfg, &paths::index_path())?))
    }

    /// Zero AUR entries — the pacman-only / not-yet-synced view.
    pub fn empty() -> Self {
        Self::from_index(IndexFile::empty())
    }

    /// Wrap an already-loaded index, building its lookup maps.
    pub fn from_index(idx: IndexFile) -> Self {
        let by = Lookup::build(&idx);
        Self {
            idx,
            by,
            label: RepoName::from(REPO_AUR),
        }
    }

    /// The loaded index — immutable for this value's lifetime.
    pub const fn index(&self) -> &IndexFile {
        &self.idx
    }

    /// Raw access to the name-lookup maps, for callers that need the maps
    /// themselves (e.g. the completion universe). Most callers want
    /// [`Self::entry`] / [`Self::search`].
    pub const fn lookup(&self) -> &Lookup {
        &self.by
    }

    /// The provenance label rows from this data carry (`aur`).
    pub const fn label(&self) -> &RepoName {
        &self.label
    }

    /// Resolve one user-typed spec — pkgname, `provides` virtual, or pkgbase,
    /// in that precedence — to its entry. The `&str` step into the lookup
    /// tables happens here, at the index's own boundary, not at call sites.
    pub fn entry(&self, spec: &PkgTarget) -> Option<&IndexEntry> {
        self.by.lookup(&self.idx, spec.as_str())
    }

    /// Regex search across pkgnames / descriptions / provides.
    pub fn search(&self, regexes: &[regex::Regex]) -> Vec<&IndexEntry> {
        self.by.search(&self.idx, regexes)
    }

    /// How a foreign (non-repo) localdb pkgname relates to this data — own
    /// pkgname / provides / pkgbase / unknown.
    pub fn classify_foreign(&self, name: &PkgName) -> AurClass<'_> {
        self.by.classify_foreign(&self.idx, name)
    }

    /// The pkgbase that owns a foreign localdb pkgname, or `None` when the
    /// name isn't in the index. Maps an AUR upgrade row (keyed by pkgname)
    /// back to the pkgbase build bookkeeping is keyed on.
    pub fn pkgbase_of(&self, name: &PkgName) -> Option<&PkgBase> {
        match self.classify_foreign(name) {
            AurClass::AsPkgname(e) | AurClass::AsProvides(e) | AurClass::AsPkgbase(e) => {
                Some(&e.pkgbase)
            }
            AurClass::NotInAur => None,
        }
    }
}

/// Atomically write the index to `path` via `index.bin.tmp` + rename:
/// the [`file_header`] followed by the rkyv archive.
#[instrument(skip(idx), fields(entries = idx.entries.len()))]
pub fn save(idx: &IndexFile, path: &Path) -> Result<()> {
    let bytes = rkyv::to_bytes::<RkyvError>(idx).map_err(|e| Error::Rkyv(e.to_string()))?;
    let tmp = path.with_extension("bin.tmp");
    let mut out = Vec::with_capacity(HEADER_LEN + bytes.len());
    out.extend_from_slice(&file_header(IndexFile::FORMAT_VERSION));
    out.extend_from_slice(&bytes);
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, path)?;
    info!(path = %path.display(), bytes = out.len(), "index saved");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(pkgbase: &str) -> IndexEntry {
        use crate::index::schema::Pkgname;
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: vec![Pkgname {
                name: pkgbase.into(),
                provides: Vec::new(),
                pkgdesc: None,
            }],
            pkgver: "1.0".into(),
            pkgrel: "1".into(),
            ..Default::default()
        }
    }

    // --- file header: version skew vs corruption, told apart at load ---

    fn incompatible_reason(path: &Path) -> String {
        match load(path).expect_err("load must fail") {
            Error::IndexIncompatible(reason) => reason,
            other => panic!("expected IndexIncompatible, got: {other}"),
        }
    }

    #[test]
    fn save_then_load_round_trips_through_the_header() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("index.bin");
        let mut idx = IndexFile::empty();
        idx.entries.push(mk("foo"));
        save(&idx, &path).unwrap();
        let loaded = load(&path).expect("fresh save must load");
        assert_eq!(loaded.entries.len(), 1);
        assert_eq!(loaded.entries[0].pkgbase, "foo");
    }

    #[test]
    fn headerless_file_reports_a_pre_header_format() {
        // Anything without the magic — a pre-v9 index, or garbage — predates
        // the header. Precise version reporting starts at v9; before that the
        // archive is opaque, so this coarse reason is the best load can do.
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("index.bin");
        std::fs::write(&path, b"this is not a valid rkyv archive at all").unwrap();
        assert_eq!(
            incompatible_reason(&path),
            format!("format predates v{}", IndexFile::FORMAT_VERSION),
        );
    }

    #[test]
    fn version_skew_reports_the_format_change_not_corruption() {
        // The normal post-upgrade case: a valid header from another format
        // version. The payload never gets validated — the header alone names
        // the skew, old → new.
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("index.bin");
        let old = IndexFile::FORMAT_VERSION - 1;
        let mut bytes = file_header(old).to_vec();
        bytes.extend_from_slice(b"payload irrelevant");
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(
            incompatible_reason(&path),
            format!("format changed (v{old} → v{})", IndexFile::FORMAT_VERSION),
        );
    }

    #[test]
    fn corrupt_payload_reports_corruption_with_a_clean_message() {
        // Header matches, archive doesn't validate: genuine corruption. The
        // reason is the fixed readable string — rkyv's rancor error (an opaque
        // "enable debug assertions…" placeholder in release builds) must not
        // leak into it.
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("index.bin");
        let mut bytes = file_header(IndexFile::FORMAT_VERSION).to_vec();
        bytes.extend_from_slice(b"this is not a valid rkyv archive at all");
        std::fs::write(&path, &bytes).unwrap();
        let reason = incompatible_reason(&path);
        assert_eq!(reason, "on-disk archive corrupted");
        assert!(
            !reason.contains("debug assertions") && !reason.contains("rancor"),
            "rancor placeholder leaked: {reason}",
        );
    }
}
