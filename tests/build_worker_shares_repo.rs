//! Regression: the parallel index-build worker must borrow the shared
//! `&mirror.repo` handle across rayon iterations, not call `gix::open` per
//! branch. Per-iter reopen reparses config + scans refs N times and dominates
//! wall time on large mirrors (the AUR mirror has ~150k branches).
//!
//! Lives in its own integration-test binary so the
//! [`gitaur::index::build::WORKER_REPO_OPENS`] counter is isolated from other
//! integration tests sharing the same process / static.

use gitaur::config::defaults::default_config;
use gitaur::index::build::{WORKER_REPO_OPENS, full_build};
use gitaur::mirror::MirrorRepo;
use gitaur::testing::git;
use std::path::Path;
use std::sync::atomic::Ordering;
use tempfile::TempDir;

fn build_fake_mirror(root: &Path, branches: &[(&str, &str)]) -> std::path::PathBuf {
    let src = root.join("src");
    let bare = root.join("bare");
    std::fs::create_dir_all(&src).unwrap();
    git(&["init", "-q", "-b", "trunk"], &src);
    for (i, (branch, srcinfo)) in branches.iter().enumerate() {
        git(&["checkout", "-q", "-b", branch], &src);
        std::fs::write(src.join(".SRCINFO"), srcinfo).unwrap();
        git(&["add", "-A"], &src);
        git(&["commit", "-q", "-m", &format!("c{i}")], &src);
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
    bare
}

#[test]
fn worker_does_not_reopen_repo_per_branch() {
    let dir = TempDir::new().unwrap();
    let bare = build_fake_mirror(
        dir.path(),
        &[
            ("a", "pkgbase = a\npkgver = 1\npkgrel = 1\npkgname = a\n"),
            ("b", "pkgbase = b\npkgver = 1\npkgrel = 1\npkgname = b\n"),
            ("c", "pkgbase = c\npkgver = 1\npkgrel = 1\npkgname = c\n"),
            ("d", "pkgbase = d\npkgver = 1\npkgrel = 1\npkgname = d\n"),
            ("e", "pkgbase = e\npkgver = 1\npkgrel = 1\npkgname = e\n"),
        ],
    );

    WORKER_REPO_OPENS.store(0, Ordering::Relaxed);

    let cfg = default_config();
    let mirror = MirrorRepo::open(&bare).unwrap();
    let idx = full_build(&cfg, &mirror).unwrap();
    assert_eq!(idx.entries.len(), 5);

    let opens = WORKER_REPO_OPENS.load(Ordering::Relaxed);
    assert_eq!(
        opens, 0,
        "worker called gix::open {opens}× — it must reuse &mirror.repo instead",
    );
}
