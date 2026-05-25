//! Integration: `-Sy` must self-heal when the on-disk `index.bin` is from an
//! older schema (or otherwise unreadable) instead of bubbling up an rkyv
//! error and trapping the user in a "run -Sy to rebuild" loop.
//!
//! Scenario: a bootstrapped bare mirror + a corrupted index file. Running
//! `mirror::cmd_refresh` must:
//!   1. notice the load failure,
//!   2. log a warning,
//!   3. rebuild via `full_build`, leaving `index.bin` loadable on the next
//!      invocation.
//!
//! `paths::state_dir()` is rerouted into a tempdir via the TLS-backed
//! [`ScopedStateRoot`] guard so no env-var mutation is needed and the
//! override is auto-cleared on test exit.

use gitaur::config::defaults::default_config;
use gitaur::config::Config;
use gitaur::index::{self, IndexFile};
use gitaur::mirror;
use gitaur::names::PkgBase;
use gitaur::paths;
use gitaur::runopts::{self, RunOpts};
use gitaur::testing::{git, ScopedStateRoot};
use std::path::Path;
use tempfile::TempDir;

/// Build a non-bare repo with one branch per (pkgbase, .SRCINFO content)
/// tuple, then clone it bare alongside. The bare becomes the upstream
/// `cfg.mirror_url` points at — gitaur's bootstrap path drives the rest,
/// including the gix refspec config that `incremental_fetch` later needs.
fn build_upstream_bare(root: &Path, branches: &[(&str, &str)]) -> std::path::PathBuf {
    let src = root.join("src");
    let bare = root.join("upstream.bare");
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

/// The two fixture branches every test in this file indexes — a plain pkgbase
/// and a split pkg with a per-pkgname `provides`.
const FIXTURE_BRANCHES: &[(&str, &str)] = &[
    (
        "cower",
        "pkgbase = cower\npkgver = 17\npkgrel = 2\npkgname = cower\n",
    ),
    (
        "bisq",
        "pkgbase = bisq\n\
         pkgver = 1\npkgrel = 1\n\
         pkgname = bisq-desktop\n\
            provides = bisq\n",
    ),
];

/// Bootstrap a mirror into a fresh tempdir state root, then overwrite the
/// freshly-built index with bytes rkyv can't validate — the post-schema-bump
/// failure mode. Returns the live `TempDir` + [`ScopedStateRoot`] guards the
/// caller must keep in scope, plus the configured [`Config`].
fn bootstrapped_with_corrupt_index() -> (TempDir, ScopedStateRoot, Config) {
    let td = TempDir::new().unwrap();
    let state_root = td.path().join("state");
    std::fs::create_dir_all(&state_root).unwrap();
    let guard = ScopedStateRoot::new(state_root);

    let upstream = build_upstream_bare(td.path(), FIXTURE_BRANCHES);
    let mut cfg = default_config();
    cfg.mirror_url = format!("file://{}", upstream.display());

    mirror::cmd_refresh(&cfg, false).expect("initial bootstrap must succeed");
    let idx_path = paths::index_path();
    std::fs::write(&idx_path, b"this is not a valid rkyv archive at all").unwrap();
    assert!(
        index::load(&idx_path).is_err(),
        "precondition: planted index must be unreadable",
    );

    (td, guard, cfg)
}

#[test]
fn load_or_resync_rebuilds_and_returns_index() {
    // Default opts (no --noresync): a normal command that hits the bad index
    // transparently resyncs and gets a usable index back, no `-Sy` by hand.
    runopts::set(RunOpts::default());
    let (_td, _guard, cfg) = bootstrapped_with_corrupt_index();

    let idx = index::load_or_resync(&cfg, &paths::index_path())
        .expect("load_or_resync must rebuild and return the index");
    assert_eq!(idx.format_version, IndexFile::FORMAT_VERSION);
    let mut bases: Vec<&PkgBase> = idx.entries.iter().map(|e| &e.pkgbase).collect();
    bases.sort_unstable();
    assert_eq!(bases, vec![&PkgBase::from("bisq"), &PkgBase::from("cower")]);
}

#[test]
fn load_or_resync_honors_noresync_and_does_not_rebuild() {
    // --noresync set: surface the incompatibility as an error instead of
    // kicking off an implicit network fetch + rebuild.
    runopts::set(RunOpts {
        noconfirm: false,
        noresync: true,
    });
    let (_td, _guard, cfg) = bootstrapped_with_corrupt_index();
    let idx_path = paths::index_path();
    let planted = std::fs::read(&idx_path).unwrap();

    let err = index::load_or_resync(&cfg, &idx_path)
        .expect_err("--noresync must surface the error rather than rebuild");
    assert!(
        format!("{err}").contains("noresync"),
        "error should point at --noresync: {err}",
    );
    // The corrupt bytes are untouched — confirms no rebuild slipped through.
    assert_eq!(
        std::fs::read(&idx_path).unwrap(),
        planted,
        "index must be left as-is under --noresync",
    );

    // This binary's tests share runner threads; clear the override so a later
    // test on this thread starts from the default.
    runopts::set(RunOpts::default());
}

#[test]
fn cmd_refresh_rebuilds_when_existing_index_is_unreadable() {
    let td = TempDir::new().unwrap();
    let state_root = td.path().join("state");
    std::fs::create_dir_all(&state_root).unwrap();

    // Reroute paths::state_dir() into the tempdir for the rest of this test.
    // The guard restores the previous value on drop (incl. panic), so we
    // can't strand the override.
    let _guard = ScopedStateRoot::new(state_root.clone());
    assert_eq!(paths::state_dir(), state_root);

    let upstream = build_upstream_bare(
        td.path(),
        &[
            (
                "cower",
                "pkgbase = cower\npkgver = 17\npkgrel = 2\npkgname = cower\n",
            ),
            (
                "bisq",
                "pkgbase = bisq\n\
                 pkgver = 1\npkgrel = 1\n\
                 pkgname = bisq-desktop\n\
                    provides = bisq\n",
            ),
        ],
    );

    let mut cfg = default_config();
    cfg.mirror_url = format!("file://{}", upstream.display());

    // First refresh: bootstraps the state_dir/aur clone and writes a fresh
    // index. This sets up `incremental_fetch`'s refspec config for the
    // recovery-path call below.
    mirror::cmd_refresh(&cfg, false).expect("initial bootstrap must succeed");
    let idx_path = paths::index_path();
    assert!(idx_path.exists(), "bootstrap must write index.bin");

    // Now corrupt the index. Random bytes that rkyv cannot validate match
    // the real failure mode after a schema bump: the file is on disk,
    // owned by us, but the layout no longer parses.
    std::fs::write(&idx_path, b"this is not a valid rkyv archive at all").unwrap();
    assert!(
        index::load(&idx_path).is_err(),
        "precondition: planted index must be unreadable",
    );

    // Second refresh: bare is bootstrapped, fetch returns no updates, the
    // existing index fails to load → recovery branch fires.
    mirror::cmd_refresh(&cfg, false).expect("cmd_refresh must recover from a bad index");

    let idx = index::load(&idx_path).expect("rebuilt index must be loadable");
    assert_eq!(idx.format_version, IndexFile::FORMAT_VERSION);
    assert_eq!(idx.entries.len(), 2, "both fixture branches indexed");
    let mut bases: Vec<&PkgBase> = idx.entries.iter().map(|e| &e.pkgbase).collect();
    bases.sort_unstable();
    assert_eq!(bases, vec![&PkgBase::from("bisq"), &PkgBase::from("cower")]);

    // Per-pkgname provides survived the rebuild — sanity for the schema
    // change that prompted this recovery path in the first place.
    let bisq = idx.entries.iter().find(|e| e.pkgbase == "bisq").unwrap();
    assert_eq!(bisq.pkgnames.len(), 1);
    assert_eq!(bisq.pkgnames[0].name, "bisq-desktop");
    assert_eq!(bisq.pkgnames[0].provides, vec!["bisq".to_owned()]);
}

#[test]
fn scoped_state_root_restores_previous_value_on_drop() {
    // Regression: a stale override would silently reroute *every* subsequent
    // test on the same runner thread. The RAII guard must restore None
    // (or the previous value) on drop, even after we did real work inside.
    let default_dir = {
        // Take the un-overridden state_dir as the baseline before any guard
        // is in scope. This snapshot reflects whatever XDG_STATE_HOME points
        // at on the host running the test, which is fine for an
        // equality check at drop time.
        paths::state_dir()
    };

    let td = TempDir::new().unwrap();
    let scoped = td.path().to_path_buf();
    {
        let _guard = ScopedStateRoot::new(scoped.clone());
        assert_eq!(paths::state_dir(), scoped);
    }
    assert_eq!(
        paths::state_dir(),
        default_dir,
        "guard must restore the pre-override value on drop",
    );
}
