//! Regression: `bootstrap_clone` must land branches under `refs/heads/*`,
//! matching `git clone --bare` semantics. gix's default refspec is
//! `+refs/heads/*:refs/remotes/<name>/*` (the non-bare git-clone default),
//! which broke `is_bootstrapped()` and `collect_branches()` — both scan
//! `refs/heads/*`.

use aurox::config::defaults::default_config;
use aurox::mirror::clone::bootstrap_clone;
use aurox::testing::git;
use indicatif::MultiProgress;
use tempfile::TempDir;

#[test]
fn bootstrap_clone_lands_refs_under_refs_heads() {
    let root = TempDir::new().unwrap();
    let src = root.path().join("src");
    let bare = root.path().join("bare");
    let dest = root.path().join("dest");

    // Source repo: two branches, mimicking the github.com/archlinux/aur
    // layout where each AUR pkgbase is its own `refs/heads/<pkgbase>`.
    std::fs::create_dir_all(&src).unwrap();
    git(&["init", "-q", "-b", "trunk"], &src);
    git(&["config", "user.email", "test@example.com"], &src);
    git(&["config", "user.name", "test"], &src);
    for branch in ["pkg-a", "pkg-b"] {
        git(&["checkout", "-q", "-b", branch], &src);
        std::fs::write(src.join(".SRCINFO"), format!("pkgbase = {branch}\n")).unwrap();
        git(&["add", "-A"], &src);
        git(&["commit", "-q", "-m", branch], &src);
    }
    git(
        &[
            "clone",
            "-q",
            "--bare",
            src.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
        root.path(),
    );

    // Point aurox at the bare and run the real bootstrap path.
    let mut cfg = default_config();
    cfg.mirror_url = format!("file://{}", bare.display());
    bootstrap_clone(&cfg, &dest, &MultiProgress::new()).expect("bootstrap_clone");

    // Collect every ref under both candidate roots.
    let repo = gix::open(&dest).expect("open dest");
    let mut heads: Vec<String> = repo
        .references()
        .unwrap()
        .prefixed("refs/heads/")
        .unwrap()
        .map(|r| r.unwrap().name().shorten().to_string())
        .collect();
    heads.sort();
    // gix may leave a symbolic `refs/remotes/origin/HEAD` pointer in place —
    // harmless metadata that doesn't appear when listing branches. What the
    // bug was about: real branch refs being duplicated under
    // `refs/remotes/origin/<branch>`. That must not happen.
    let leaked_branches: Vec<String> = repo
        .references()
        .unwrap()
        .prefixed("refs/remotes/origin/")
        .unwrap()
        .filter_map(|r| {
            let r = r.unwrap();
            let name = r.name().shorten().to_string();
            (name != "HEAD" && name != "origin/HEAD").then_some(name)
        })
        .collect();

    assert!(
        heads.contains(&"pkg-a".to_owned()) && heads.contains(&"pkg-b".to_owned()),
        "expected pkg-a and pkg-b under refs/heads/, got: {heads:?}",
    );
    assert!(
        leaked_branches.is_empty(),
        "bare clone must not duplicate branch refs under refs/remotes/, got: {leaked_branches:?}",
    );
}
