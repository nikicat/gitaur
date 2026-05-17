//! In-memory index of AUR pkgbases, persisted as a single rkyv-archived file.
//!
//! Loaded via mmap with no per-entry deserialization. Secondary hashmaps
//! (`by_name`, `by_provides`) are built post-load in [`secondary`].

use crate::config::Config;
use crate::error::{Error, Result};
use crate::paths;
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
#[instrument]
pub fn load(path: &Path) -> Result<IndexFile> {
    if !path.exists() {
        debug!("index missing; returning empty");
        return Ok(IndexFile::empty());
    }
    let bytes = std::fs::read(path)?;
    let idx: IndexFile =
        rkyv::from_bytes::<IndexFile, RkyvError>(&bytes).map_err(|e| Error::Rkyv(e.to_string()))?;
    info!(entries = idx.entries.len(), "index loaded");
    Ok(idx)
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
pub fn cmd_search(_cfg: &Config, terms: &[String]) -> Result<u8> {
    let path = paths::index_path();
    if !path.exists() {
        ui::warn("no index; run `gitaur -Sy` first");
        return Ok(1);
    }
    let idx = load(&path)?;
    let by = secondary::Secondary::build(&idx);
    let regexes: Vec<regex::Regex> = terms
        .iter()
        .map(|t| regex::RegexBuilder::new(t).case_insensitive(true).build())
        .collect::<std::result::Result<_, _>>()?;
    let hits = by.search(&idx, &regexes);
    info!(count = hits.len(), "search results");
    for entry in hits {
        let head = format!(
            "aur/{} {}",
            entry.pkgbase,
            version_string(entry.epoch.as_ref(), &entry.pkgver, &entry.pkgrel)
        );
        ui::info(&head);
        if let Some(desc) = &entry.pkgdesc {
            println!("    {desc}");
        }
    }
    Ok(0)
}

/// `-Si` info for one or more pkgnames.
pub fn cmd_info(_cfg: &Config, targets: &[String]) -> Result<u8> {
    let idx = load(&paths::index_path())?;
    let by = secondary::Secondary::build(&idx);
    let mut missing = Vec::new();
    for t in targets {
        let Some(entry) = by.lookup(&idx, t) else {
            missing.push(t.clone());
            continue;
        };
        print_info(entry);
    }
    if !missing.is_empty() {
        ui::warn(&format!("not in AUR: {}", missing.join(", ")));
        return Ok(1);
    }
    Ok(0)
}

fn print_info(e: &IndexEntry) {
    println!("Repository      : aur");
    println!("Name            : {}", e.pkgbase);
    if !e.pkgnames.is_empty() && e.pkgnames != vec![e.pkgbase.clone()] {
        println!("Split pkgs      : {}", e.pkgnames.join(" "));
    }
    println!(
        "Version         : {}",
        version_string(e.epoch.as_ref(), &e.pkgver, &e.pkgrel)
    );
    if let Some(d) = &e.pkgdesc {
        println!("Description     : {d}");
    }
    if !e.depends.is_empty() {
        println!("Depends On      : {}", e.depends.join(" "));
    }
    if !e.makedepends.is_empty() {
        println!("Make Deps       : {}", e.makedepends.join(" "));
    }
    if !e.provides.is_empty() {
        println!("Provides        : {}", e.provides.join(" "));
    }
    println!();
}

fn version_string(epoch: Option<&String>, ver: &str, rel: &str) -> String {
    match epoch {
        Some(e) if !e.is_empty() => format!("{e}:{ver}-{rel}"),
        _ => format!("{ver}-{rel}"),
    }
}
