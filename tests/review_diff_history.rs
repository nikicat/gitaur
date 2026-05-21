//! Integration: walk a bare-mirror history to find the commit whose
//! `.SRCINFO` declares a target installed version. Builds a tiny multi-commit
//! repo with the system `git` CLI, then drives [`review::find_installed_commit`]
//! against it.

use gitaur::build::review;
use gitaur::mirror::MirrorRepo;
use gitaur::testing::{git, git_stdout};
use gix::ObjectId;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Build a bare repo whose `refs/heads/<pkgbase>` walks through `versions`
/// (oldest → newest), with one commit per entry. Each commit's `.SRCINFO`
/// declares the entry's `(pkgver, pkgrel[, epoch])`. Returns `(bare path,
/// commit OIDs in order)`.
fn build_history(
    root: &Path,
    pkgbase: &str,
    versions: &[Version<'_>],
) -> (PathBuf, Vec<ObjectId>) {
    let src = root.join("src");
    let bare = root.join("bare");
    std::fs::create_dir_all(&src).unwrap();
    git(&["init", "-q", "-b", pkgbase], &src);

    let mut oids = Vec::with_capacity(versions.len());
    for (i, v) in versions.iter().enumerate() {
        std::fs::write(src.join(".SRCINFO"), v.srcinfo(pkgbase)).unwrap();
        // PKGBUILD content is the diff payload; we just need *some* bytes
        // that change per commit so the diff path has something to show
        // when exercised (this test doesn't assert on it, but other tests
        // building on this helper might).
        std::fs::write(
            src.join("PKGBUILD"),
            format!(
                "pkgname={pkgbase}\npkgver={}\npkgrel={}\n",
                v.pkgver, v.pkgrel
            ),
        )
        .unwrap();
        git(&["add", "-A"], &src);
        git(&["commit", "-q", "-m", &format!("v{i}")], &src);
        let hex = git_stdout(&["rev-parse", "HEAD"], &src);
        oids.push(ObjectId::from_hex(hex.trim().as_bytes()).unwrap());
    }
    git(
        &[
            "clone",
            "-q",
            "--bare",
            src.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
        root,
    );
    (bare, oids)
}

struct Version<'a> {
    pkgver: &'a str,
    pkgrel: &'a str,
    epoch: Option<&'a str>,
}

impl<'a> Version<'a> {
    const fn new(pkgver: &'a str, pkgrel: &'a str) -> Self {
        Self {
            pkgver,
            pkgrel,
            epoch: None,
        }
    }

    const fn with_epoch(pkgver: &'a str, pkgrel: &'a str, epoch: &'a str) -> Self {
        Self {
            pkgver,
            pkgrel,
            epoch: Some(epoch),
        }
    }

    fn srcinfo(&self, pkgbase: &str) -> String {
        use std::fmt::Write as _;
        let mut s = format!("pkgbase = {pkgbase}\n");
        if let Some(e) = self.epoch {
            writeln!(s, "\tepoch = {e}").unwrap();
        }
        writeln!(s, "\tpkgver = {}", self.pkgver).unwrap();
        writeln!(s, "\tpkgrel = {}", self.pkgrel).unwrap();
        writeln!(s, "\npkgname = {pkgbase}").unwrap();
        s
    }
}

#[test]
fn finds_middle_commit_for_installed_version() {
    let dir = TempDir::new().unwrap();
    let (bare, oids) = build_history(
        dir.path(),
        "foo",
        &[
            Version::new("1.0", "1"),
            Version::new("1.1", "1"),
            Version::new("1.2", "1"),
        ],
    );
    let mirror = MirrorRepo::open(&bare).unwrap();
    let head = oids[2];

    let found = review::find_installed_commit(&mirror, head, "1.1-1").unwrap();
    assert_eq!(found, Some(oids[1]));
}

#[test]
fn finds_oldest_commit_for_installed_version() {
    let dir = TempDir::new().unwrap();
    let (bare, oids) = build_history(
        dir.path(),
        "foo",
        &[
            Version::new("1.0", "1"),
            Version::new("1.1", "1"),
            Version::new("1.2", "1"),
        ],
    );
    let mirror = MirrorRepo::open(&bare).unwrap();
    let head = oids[2];

    let found = review::find_installed_commit(&mirror, head, "1.0-1").unwrap();
    assert_eq!(found, Some(oids[0]));
}

#[test]
fn returns_none_when_version_not_in_history() {
    let dir = TempDir::new().unwrap();
    let (bare, oids) = build_history(
        dir.path(),
        "foo",
        &[Version::new("1.0", "1"), Version::new("1.1", "1")],
    );
    let mirror = MirrorRepo::open(&bare).unwrap();
    let head = oids[1];

    let found = review::find_installed_commit(&mirror, head, "9.9-9").unwrap();
    assert_eq!(found, None);
}

#[test]
fn matches_version_with_epoch_prefix() {
    // The `[epoch:]pkgver-pkgrel` form: alpm's `installed_version` returns
    // `2:0.1-1` when epoch is set, so the SRCINFO at that commit must
    // round-trip to the same string via `IndexEntry::version`.
    let dir = TempDir::new().unwrap();
    let (bare, oids) = build_history(
        dir.path(),
        "foo",
        &[
            Version::with_epoch("0.1", "1", "1"),
            Version::with_epoch("0.1", "1", "2"),
        ],
    );
    let mirror = MirrorRepo::open(&bare).unwrap();
    let head = oids[1];

    let found = review::find_installed_commit(&mirror, head, "1:0.1-1").unwrap();
    assert_eq!(found, Some(oids[0]));
}

#[test]
fn picks_head_when_already_at_installed_version() {
    // Reinstall case is short-circuited by the caller, but if it ever does
    // call us with installed == new_ver we should still resolve to HEAD
    // (not silently skip past it).
    let dir = TempDir::new().unwrap();
    let (bare, oids) = build_history(
        dir.path(),
        "foo",
        &[Version::new("1.0", "1"), Version::new("1.1", "1")],
    );
    let mirror = MirrorRepo::open(&bare).unwrap();
    let head = oids[1];

    let found = review::find_installed_commit(&mirror, head, "1.1-1").unwrap();
    assert_eq!(found, Some(oids[1]));
}
