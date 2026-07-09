//! Integration: walk a bare-mirror history to find the commit whose
//! `.SRCINFO` declares a target installed version. Builds a tiny multi-commit
//! repo with the system `git` CLI, then drives [`review::find_installed_commit`]
//! against it.

use aurox::build::review;
use aurox::build::review::HistorySearch;
use aurox::mirror::MirrorRepo;
use aurox::testing::{git, git_stdout};
use aurox::version::Ver;
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
    versions: &[SrcinfoFixture<'_>],
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

struct SrcinfoFixture<'a> {
    pkgver: &'a str,
    pkgrel: &'a str,
    epoch: Option<&'a str>,
}

impl<'a> SrcinfoFixture<'a> {
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
            SrcinfoFixture::new("1.0", "1"),
            SrcinfoFixture::new("1.1", "1"),
            SrcinfoFixture::new("1.2", "1"),
        ],
    );
    let mirror = MirrorRepo::open(&bare).unwrap();
    let head = oids[2];

    let found = review::find_installed_commit(&mirror, head, Ver::new("1.1-1"), 64).unwrap();
    assert_eq!(found, HistorySearch::Found(oids[1]));
}

#[test]
fn finds_oldest_commit_for_installed_version() {
    let dir = TempDir::new().unwrap();
    let (bare, oids) = build_history(
        dir.path(),
        "foo",
        &[
            SrcinfoFixture::new("1.0", "1"),
            SrcinfoFixture::new("1.1", "1"),
            SrcinfoFixture::new("1.2", "1"),
        ],
    );
    let mirror = MirrorRepo::open(&bare).unwrap();
    let head = oids[2];

    let found = review::find_installed_commit(&mirror, head, Ver::new("1.0-1"), 64).unwrap();
    assert_eq!(found, HistorySearch::Found(oids[0]));
}

#[test]
fn returns_none_when_version_not_in_history() {
    let dir = TempDir::new().unwrap();
    let (bare, oids) = build_history(
        dir.path(),
        "foo",
        &[
            SrcinfoFixture::new("1.0", "1"),
            SrcinfoFixture::new("1.1", "1"),
        ],
    );
    let mirror = MirrorRepo::open(&bare).unwrap();
    let head = oids[1];

    let found = review::find_installed_commit(&mirror, head, Ver::new("9.9-9"), 64).unwrap();
    // Branch only has 2 commits; walk reaches root well under the bound.
    assert_eq!(found, HistorySearch::NotInLineage { walked: 2 });
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
            SrcinfoFixture::with_epoch("0.1", "1", "1"),
            SrcinfoFixture::with_epoch("0.1", "1", "2"),
        ],
    );
    let mirror = MirrorRepo::open(&bare).unwrap();
    let head = oids[1];

    let found = review::find_installed_commit(&mirror, head, Ver::new("1:0.1-1"), 64).unwrap();
    assert_eq!(found, HistorySearch::Found(oids[0]));
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
        &[
            SrcinfoFixture::new("1.0", "1"),
            SrcinfoFixture::new("1.1", "1"),
        ],
    );
    let mirror = MirrorRepo::open(&bare).unwrap();
    let head = oids[1];

    let found = review::find_installed_commit(&mirror, head, Ver::new("1.1-1"), 64).unwrap();
    assert_eq!(found, HistorySearch::Found(oids[1]));
}

/// The dotnet-runtime-7.0 regression that motivated the configurable
/// `review_history_scan_max`: the target version DOES exist in the
/// pkgbase's history but sits past the search bound. With a too-small
/// bound the walk misses it; with a large-enough bound it finds it.
#[test]
fn bound_governs_how_far_back_we_look() {
    let dir = TempDir::new().unwrap();
    // Build a 5-deep history; the matching commit (sdk120) is at depth 4
    // (oldest), with sdk130, sdk140, sdk150, and sdk160 stacked above.
    let (bare, oids) = build_history(
        dir.path(),
        "dotnet-core-7.0-bin",
        &[
            SrcinfoFixture::new("7.0.20.sdk120", "2"),
            SrcinfoFixture::new("7.0.20.sdk130", "1"),
            SrcinfoFixture::new("7.0.20.sdk140", "1"),
            SrcinfoFixture::new("7.0.20.sdk150", "1"),
            SrcinfoFixture::new("7.0.20.sdk160", "1"),
        ],
    );
    let mirror = MirrorRepo::open(&bare).unwrap();
    let head = oids[4];

    // Bound = 3 → walk visits sdk160, sdk150, sdk140; consumes the full
    // bound without finding sdk120. `BoundExceeded` distinguishes this
    // from "branch fully walked".
    let missed =
        review::find_installed_commit(&mirror, head, Ver::new("7.0.20.sdk120-2"), 3).unwrap();
    assert_eq!(
        missed,
        HistorySearch::BoundExceeded { bound: 3 },
        "bound=3 must report BoundExceeded — the commit may still exist past the bound"
    );

    // Bound = 5 → walk reaches the oldest commit, finds it.
    let found =
        review::find_installed_commit(&mirror, head, Ver::new("7.0.20.sdk120-2"), 5).unwrap();
    assert_eq!(
        found,
        HistorySearch::Found(oids[0]),
        "bound=5 must reach the depth-4 match"
    );

    // Bound = 100 (way larger than the branch) → walk runs out of
    // ancestors. `NotInLineage` with the actual walk depth = 4 (sdk160,
    // sdk150, sdk140, sdk130) so the fallback note can show "4
    // ancestor(s)" — no false bump-the-bound advice.
    let exhausted =
        review::find_installed_commit(&mirror, head, Ver::new("9.9.9.sdkXXX-1"), 100).unwrap();
    assert_eq!(
        exhausted,
        HistorySearch::NotInLineage { walked: 5 },
        "bound > branch length must report NotInLineage — bumping the bound wouldn't help"
    );
}
