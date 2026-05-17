//! Integration: build a tiny bare repo with a few branches, then exercise
//! the full index build + load + incremental update pipeline against it.
//!
//! Uses system `git` to build the fixture repos (gix 0.83 doesn't expose
//! high-level init+commit plumbing). The code under test is gix-driven.

use gitaur::config::defaults::default_config;
use gitaur::index::build::full_build;
use gitaur::index::secondary::Secondary;
use gitaur::index::update::incremental_update;
use gitaur::index::{load, save};
use gitaur::mirror::fetch::RefUpdate;
use gitaur::mirror::MirrorRepo;
use gitaur::testing::{git, git_stdout};
use std::path::Path;
use tempfile::TempDir;

/// Build a bare repo whose `refs/heads/<branch>` tip is a commit containing a
/// single `.SRCINFO` blob with the given content.
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

/// Push a new commit to a branch in the source repo, then mirror it into the bare.
fn update_branch(root: &Path, branch: &str, srcinfo: &str) -> gix::ObjectId {
    let src = root.join("src");
    let bare = root.join("bare");
    git(&["checkout", "-q", branch], &src);
    std::fs::write(src.join(".SRCINFO"), srcinfo).unwrap();
    git(&["add", "-A"], &src);
    git(&["commit", "-q", "-m", "update"], &src);
    let hex = git_stdout(&["rev-parse", branch], &src);
    let hex = hex.trim();
    git(
        &[
            "push",
            "-q",
            "--force",
            bare.to_str().unwrap(),
            &format!("{branch}:{branch}"),
        ],
        &src,
    );
    gix::ObjectId::from_hex(hex.as_bytes()).unwrap()
}

#[test]
fn full_build_and_lookup() {
    let dir = TempDir::new().unwrap();
    let bare = build_fake_mirror(
        dir.path(),
        &[
            (
                "cower",
                "pkgbase = cower\npkgver = 17\npkgrel = 2\npkgname = cower\ndepends = curl\n",
            ),
            (
                "paru-bin",
                "pkgbase = paru-bin\npkgver = 2\npkgrel = 1\npkgname = paru-bin\nprovides = paru\n",
            ),
            (
                "yay",
                "pkgbase = yay\npkgver = 12\npkgrel = 1\npkgname = yay\nmakedepends = go\n",
            ),
        ],
    );

    let cfg = default_config();
    let mirror = MirrorRepo::open(&bare).unwrap();
    let idx = full_build(&cfg, &mirror).unwrap();
    assert_eq!(idx.entries.len(), 3);

    let secondary = Secondary::build(&idx);
    assert!(secondary.lookup(&idx, "cower").is_some());
    assert_eq!(
        secondary.lookup(&idx, "paru").unwrap().pkgbase,
        "paru-bin",
        "provides lookup",
    );
}

#[test]
fn save_load_roundtrip() {
    let dir = TempDir::new().unwrap();
    let bare = build_fake_mirror(
        dir.path(),
        &[(
            "cower",
            "pkgbase = cower\npkgver = 17\npkgrel = 2\npkgname = cower\n",
        )],
    );

    let cfg = default_config();
    let mirror = MirrorRepo::open(&bare).unwrap();
    let idx = full_build(&cfg, &mirror).unwrap();

    let path = dir.path().join("index.bin");
    save(&idx, &path).unwrap();
    let loaded = load(&path).unwrap();
    assert_eq!(loaded.entries.len(), idx.entries.len());
    assert_eq!(loaded.entries[0].pkgbase, idx.entries[0].pkgbase);
}

#[test]
fn incremental_update_upserts_and_deletes() {
    let dir = TempDir::new().unwrap();
    let bare = build_fake_mirror(
        dir.path(),
        &[
            (
                "cower",
                "pkgbase = cower\npkgver = 17\npkgrel = 2\npkgname = cower\n",
            ),
            (
                "yay",
                "pkgbase = yay\npkgver = 12\npkgrel = 1\npkgname = yay\n",
            ),
        ],
    );

    let cfg = default_config();
    let mirror = MirrorRepo::open(&bare).unwrap();
    let mut idx = full_build(&cfg, &mirror).unwrap();
    assert_eq!(idx.entries.len(), 2);

    let new_tip = update_branch(
        dir.path(),
        "cower",
        "pkgbase = cower\npkgver = 18\npkgrel = 1\npkgname = cower\n",
    );
    let mirror = MirrorRepo::open(&bare).unwrap();
    let update = RefUpdate {
        refname: "refs/heads/cower".into(),
        old_oid: None,
        new_oid: Some(new_tip),
    };
    incremental_update(&mirror, &[update], &mut idx).unwrap();
    let cower = idx.entries.iter().find(|e| e.pkgbase == "cower").unwrap();
    assert_eq!(cower.pkgver, "18");

    let del = RefUpdate {
        refname: "refs/heads/yay".into(),
        old_oid: None,
        new_oid: None,
    };
    incremental_update(&mirror, &[del], &mut idx).unwrap();
    assert!(idx.entries.iter().all(|e| e.pkgbase != "yay"));
}
