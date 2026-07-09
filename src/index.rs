//! In-memory index of AUR pkgbases, persisted as a single rkyv-archived file.
//!
//! Loaded via mmap with no per-entry deserialization. Secondary hashmaps
//! (`by_name`, `by_provides`) are built post-load in [`secondary`].

use crate::config::Config;
use crate::error::{Error, Result};
use crate::mirror;
use crate::names::{PkgTarget, SearchTerm};
use crate::paths;
use crate::runopts;
use crate::ui;
use rkyv::rancor::Error as RkyvError;
use std::path::Path;
use tracing::{debug, info, instrument};

pub mod build;
pub mod schema;
pub mod secondary;
pub mod srcinfo;
pub mod update;

pub use schema::{IndexEntry, IndexFile};

/// Load the on-disk index. Returns an empty index if the file is missing.
///
/// Archives this build can't read — rkyv's validator trips on a changed layout,
/// or `format_version` predates us — surface as [`Error::IndexIncompatible`].
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
    let idx: IndexFile = rkyv::from_bytes::<IndexFile, RkyvError>(&bytes).map_err(|e| {
        // A schema bump invalidates the on-disk layout, so rkyv's validator
        // trips before we'd ever see `format_version` — hence the generic
        // "unreadable" wording rather than a version comparison. The rancor
        // error carries no detail in release builds ("enable debug assertions
        // and the `alloc` feature…"), so keep it for traces and surface a
        // clean reason to the user.
        debug!(error = %e, "rkyv rejected on-disk index");
        Error::IndexIncompatible("on-disk archive unreadable".into())
    })?;
    if idx.format_version != IndexFile::FORMAT_VERSION {
        return Err(Error::IndexIncompatible(format!(
            "index format v{} predates this aurox (v{})",
            idx.format_version,
            IndexFile::FORMAT_VERSION,
        )));
    }
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
            mirror::cmd_refresh(cfg, false)?;
            load(path)
        }
        other => other,
    }
}

/// Atomically write the index to `path` via `index.bin.tmp` + rename.
#[instrument(skip(idx), fields(entries = idx.entries.len()))]
pub fn save(idx: &IndexFile, path: &Path) -> Result<()> {
    let bytes = rkyv::to_bytes::<RkyvError>(idx).map_err(|e| Error::Rkyv(e.to_string()))?;
    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    info!(path = %path.display(), bytes = bytes.len(), "index saved");
    Ok(())
}

/// `-Ss` search across pkgnames/pkgdesc/provides, with pacman-style output.
pub fn cmd_search(cfg: &Config, terms: &[SearchTerm]) -> Result<u8> {
    let path = paths::index_path();
    if !path.exists() {
        ui::warn("no index; run `aurox -Sy` first");
        return Ok(1);
    }
    let idx = load_or_resync(cfg, &path)?;
    let by = secondary::Secondary::build(&idx);
    let regexes: Vec<regex::Regex> = terms
        .iter()
        .map(SearchTerm::compile)
        .collect::<std::result::Result<_, _>>()?;
    let hits = by.search(&idx, &regexes);
    info!(count = hits.len(), "search results");
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for entry in hits {
        write_search_result(&mut out, entry)?;
    }
    Ok(0)
}

/// Render one search hit in pacman's `-Ss` format to `out`.
///
/// Stdout (not stderr) so `aurox -Ss foo | head` works — the equivalent
/// `pacman -Ss` also writes results to stdout. Lifted out of `cmd_search`
/// so the exact byte layout can be tested without spawning a process.
fn write_search_result<W: std::io::Write>(out: &mut W, entry: &IndexEntry) -> std::io::Result<()> {
    writeln!(
        out,
        "aur/{} {}",
        entry.pkgbase,
        version_string(entry.epoch.as_ref(), &entry.pkgver, &entry.pkgrel)
    )?;
    if let Some(desc) = entry.display_desc() {
        writeln!(out, "    {desc}")?;
    }
    Ok(())
}

/// `-Si` info for one or more targets (AUR-only by design — repo packages are
/// `pacman -Si`'s job on this path; the interactive shell merges the two).
pub fn cmd_info(cfg: &Config, targets: &[PkgTarget]) -> Result<u8> {
    let idx = load_or_resync(cfg, &paths::index_path())?;
    let by = secondary::Secondary::build(&idx);
    let missing: Vec<&PkgTarget> = targets
        .iter()
        .filter(|t| !print_aur_info(&idx, &by, t))
        .collect();
    if !missing.is_empty() {
        ui::warn(&format!(
            "not in AUR: {}",
            missing
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    // Pacman-style exit code: non-zero when a requested target wasn't in the AUR.
    Ok(u8::from(!missing.is_empty()))
}

/// Look up one target (pkgname / provides / pkgbase, via [`secondary`]) against
/// an already-loaded index and print its `-Si`-style block. `false` ⇒ not in
/// the AUR — the caller decides how to report the miss ([`cmd_info`] warns
/// "not in AUR"; the shell first tries the sync repos and words it
/// accordingly). Shared so both surfaces resolve a name identically.
pub(crate) fn print_aur_info(
    idx: &IndexFile,
    by: &secondary::Secondary,
    target: &PkgTarget,
) -> bool {
    match by.lookup(idx, target.as_str()) {
        Some(entry) => {
            print_info(entry);
            true
        }
        None => false,
    }
}

fn print_info(e: &IndexEntry) {
    println!("Repository      : aur");
    println!("Name            : {}", e.pkgbase);
    // Show the split-pkg list whenever the entry actually has more than one
    // pkgname (or the single pkgname differs from pkgbase). Cheap join over
    // names only — provides aren't part of the `-Si` summary.
    let trivial = e.pkgnames.len() == 1 && e.pkgbase.matches_pkgname(&e.pkgnames[0].name);
    if !e.pkgnames.is_empty() && !trivial {
        // Render via `Display` directly — no Vec<&str> staging area. Avoids
        // routing each pkgname through `as_str()` just to feed `join(" ")`.
        print!("Split pkgs      :");
        for p in &e.pkgnames {
            print!(" {}", p.name);
        }
        println!();
    }
    println!(
        "Version         : {}",
        version_string(e.epoch.as_ref(), &e.pkgver, &e.pkgrel)
    );
    if let Some(d) = e.display_desc() {
        println!("Description     : {d}");
    }
    if !e.depends.is_empty() {
        println!("Depends On      : {}", e.depends.join(" "));
    }
    if !e.makedepends.is_empty() {
        println!("Make Deps       : {}", e.makedepends.join(" "));
    }
    // Union of pkgbase-level and pkgname-scoped provides — `-Si` users
    // want to see every virtual name the pkgbase makes available, not the
    // attribution.
    let provides: Vec<String> = e.all_provides().map(ToString::to_string).collect();
    if !provides.is_empty() {
        println!("Provides        : {}", provides.join(" "));
    }
    println!();
}

fn version_string(epoch: Option<&String>, ver: &str, rel: &str) -> String {
    match epoch {
        Some(e) if !e.is_empty() => format!("{e}:{ver}-{rel}"),
        _ => format!("{ver}-{rel}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(entry: &IndexEntry) -> String {
        let mut buf: Vec<u8> = Vec::new();
        write_search_result(&mut buf, entry).unwrap();
        String::from_utf8(buf).unwrap()
    }

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

    #[test]
    fn search_result_writes_to_writer_not_stdout() {
        // The function takes any Write, so it must not be coupled to stdout.
        // (If it called println! the buffer below would be empty.)
        let e = mk("foo");
        let out = render(&e);
        assert!(!out.is_empty(), "writer captured nothing");
    }

    #[test]
    fn search_result_format_matches_pacman_ss() {
        // pacman -Ss prints `repo/name version` then indented description.
        let mut e = mk("foo");
        e.pkgdesc = Some("does foo".into());
        let out = render(&e);
        assert_eq!(out, "aur/foo 1.0-1\n    does foo\n");
    }

    #[test]
    fn search_result_omits_description_block_when_none() {
        // Single-line output, no blank "    " for entries without a pkgdesc.
        let out = render(&mk("bar"));
        assert_eq!(out, "aur/bar 1.0-1\n");
    }

    #[test]
    fn search_result_includes_epoch_when_present() {
        let mut e = mk("baz");
        e.epoch = Some("2".into());
        let out = render(&e);
        assert!(out.starts_with("aur/baz 2:1.0-1\n"), "got: {out:?}");
    }

    #[test]
    fn search_result_skips_empty_epoch_string() {
        // An empty-string epoch (e.g. from `epoch=` with no value) is not
        // rendered as `:1.0-1`. Regression bait: version_string special-cases
        // it.
        let mut e = mk("qux");
        e.epoch = Some(String::new());
        let out = render(&e);
        assert!(out.starts_with("aur/qux 1.0-1\n"), "got: {out:?}");
    }
}
