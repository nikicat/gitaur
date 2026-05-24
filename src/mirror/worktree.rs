//! Linked git worktrees pointing back at the bare mirror.
//!
//! Worktree creation and reset are delegated to the system `git` CLI:
//!
//! 1. `git worktree add --detach <dest> refs/heads/<pkgbase>` builds the admin
//!    dir, the index, and the `.git` gitlink in one call — exactly what we
//!    used to hand-roll, but with a real index so plain `git` commands work
//!    inside the build dir.
//! 2. `git -C <dest> reset --hard refs/heads/<pkgbase>` refreshes tracked
//!    files to the current tip without touching anything outside the tree.
//!    `pkg/`, `src/`, `src-cache/`, and cached `.pkg.tar.zst` all survive, so
//!    the idempotency shortcut in `build::build_one` actually fires.
//! 3. `git worktree remove --force` cleans up via coreutils `rm -rf` — which
//!    chmod-then-removes, so fakeroot-staged subtrees with restrictive perms
//!    (bisq's JRE under `pkg/`, fonts, anything `chmod 0111`'d in
//!    `package()`) don't trip EACCES the way `std::fs::remove_dir_all` does.
//!
//! TODO(gix): replace the system-git shell-outs with native gix calls once
//! gix ships a high-level worktree-add API. As of gix 0.83 the
//! `gix-worktree-*` crates expose only low-level pieces (no `git worktree
//! add`/`reset --hard` at any level), so a native implementation would mean
//! re-doing index population + checkout-mode handling by hand — which is
//! exactly the trade we're trying to back out of.

use crate::error::{Error, Result};
use crate::mirror::MirrorRepo;
use crate::names::PkgBase;
use gix::ObjectId;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, instrument};

/// One pkgbase's build directory plus the commit it was materialized from.
pub struct Worktree {
    /// Materialized files live here.
    pub path: PathBuf,
    /// Commit OID we checked out — the starting point for review's history
    /// walk when finding the AUR commit that produced `installed_ver`.
    pub head_oid: ObjectId,
}

/// Create or fast-refresh a linked worktree for `branch` at `dest`.
///
/// Existing worktree → `git reset --hard` to the branch tip (tracked files
/// only). Missing or half-state → recovery `worktree prune` + `worktree add
/// --force`. Either path ends with a working linked worktree that native
/// `git` recognises.
#[instrument(skip(mirror), fields(branch = %branch))]
pub fn add_or_reset(mirror: &MirrorRepo, branch: &PkgBase, dest: &Path) -> Result<Worktree> {
    let refname = format!("refs/heads/{branch}");
    let head_oid = peel_branch(mirror, &refname)?;

    if is_linked_worktree(dest) {
        // Surgical refresh — only the index's path set changes. Untracked
        // files (pkg/, src/, *.pkg.tar.zst) survive; that's the whole point.
        run_git(&[
            "-C".as_ref(),
            dest.as_os_str(),
            "reset".as_ref(),
            "--hard".as_ref(),
            refname.as_ref(),
        ])?;
        debug!(%branch, %head_oid, "reset existing worktree to branch tip");
    } else {
        // First call, or someone deleted half the state by hand. Drop any
        // orphaned admin entry, scrub a stale dest if one is in the way,
        // then create the worktree fresh. Prune failure isn't fatal here
        // (the subsequent `worktree add` will surface anything real), but
        // log it so post-mortems can correlate with a confusing add error.
        if let Err(e) = run_git(&[
            "-C".as_ref(),
            mirror.path.as_os_str(),
            "worktree".as_ref(),
            "prune".as_ref(),
        ]) {
            debug!(error = %e, "preparatory worktree prune failed; continuing");
        }
        if dest.exists() {
            force_remove(dest)?;
        }
        run_git(&[
            "-C".as_ref(),
            mirror.path.as_os_str(),
            "worktree".as_ref(),
            "add".as_ref(),
            "--detach".as_ref(),
            dest.as_os_str(),
            refname.as_ref(),
        ])?;
        debug!(%branch, %head_oid, dest = %dest.display(), "linked worktree created");
    }

    Ok(Worktree {
        path: dest.to_path_buf(),
        head_oid,
    })
}

/// Remove a worktree's files and admin directory. Idempotent.
///
/// Strategy: scrub `dest` with [`force_remove`] (chmod-then-`remove_dir_all`,
/// the only way to escape fakeroot-staged `0111` dirs), then let
/// `git worktree prune` sweep the now-orphaned admin entry. `branch` is
/// retained for API symmetry but no longer consulted directly — the admin
/// dir's name is derived from the dest basename by git itself, and prune
/// finds it from there.
pub fn prune(mirror: &MirrorRepo, _branch: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        force_remove(dest)?;
    }
    run_git(&[
        "-C".as_ref(),
        mirror.path.as_os_str(),
        "worktree".as_ref(),
        "prune".as_ref(),
    ])?;
    Ok(())
}

/// True iff `dest` looks like a working linked worktree — its `.git` file
/// points at an admin dir that still exists. Used to choose between
/// `reset --hard` (fast path) and full recovery + `worktree add`.
fn is_linked_worktree(dest: &Path) -> bool {
    let gitfile = dest.join(".git");
    let Ok(content) = std::fs::read_to_string(&gitfile) else {
        return false;
    };
    let Some(admin) = content.strip_prefix("gitdir:") else {
        return false;
    };
    Path::new(admin.trim()).is_dir()
}

/// chmod-then-remove. Walks `path`, restores `u+rwx` on directories we own
/// (so `read_dir` and `unlinkat` work), then calls `std::fs::remove_dir_all`.
/// Mirrors what coreutils `chmod -R u+rwX … && rm -rf …` would do — neither
/// `rm -rf` alone nor `git worktree remove` chmod-on-traverse for owner.
fn force_remove(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fn fixup(path: &Path) -> std::io::Result<()> {
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.file_type().is_dir() {
            return Ok(());
        }
        let mut perms = meta.permissions();
        if perms.mode() & 0o700 != 0o700 {
            perms.set_mode(perms.mode() | 0o700);
            std::fs::set_permissions(path, perms)?;
        }
        for entry in std::fs::read_dir(path)? {
            fixup(&entry?.path())?;
        }
        Ok(())
    }
    fixup(path)?;
    std::fs::remove_dir_all(path)?;
    Ok(())
}

/// Resolve `refname` to its tip OID using the bare repo's object DB. We do
/// this via gix instead of `git rev-parse` so the hot path (every `build_one`)
/// avoids a fork.
fn peel_branch(mirror: &MirrorRepo, refname: &str) -> Result<ObjectId> {
    let mut r = mirror
        .repo
        .find_reference(refname)
        .map_err(|e| Error::Gix(format!("find_reference {refname}: {e}")))?;
    let commit = r
        .peel_to_id()
        .map_err(|e| Error::Gix(format!("peel {refname}: {e}")))?;
    Ok(commit.detach())
}

/// Run `git <args>` and convert non-zero exits into a contextful `Error::Gix`.
/// Stderr is captured so the message points at the actual git failure rather
/// than a bare "io: ... os error N".
fn run_git(args: &[&std::ffi::OsStr]) -> Result<()> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| Error::other(format!("spawn git: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let displayed: Vec<String> = args
            .iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        return Err(Error::Gix(format!(
            "git {} failed: {}",
            displayed.join(" "),
            stderr.trim(),
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::names::PkgBase;
    use crate::testing::git;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// `add_or_reset` calls a checkout/reset on a branch whose name happens
    /// to be the pkgbase in production. In these tests the branch is the
    /// repo's default `main`, so wrap it in `PkgBase` to satisfy the typed
    /// signature without pretending it's a real pkgbase.
    fn branch(name: &str) -> PkgBase {
        PkgBase::from(name)
    }

    /// Build a tiny bare repo via the system `git` CLI; gix opens it for the
    /// `MirrorRepo` wrapper. Both sides are real, no hand-built admin dirs.
    fn make_bare(dir: &Path) -> (PathBuf, gix::Repository) {
        let src = dir.join("src");
        let bare = dir.join("bare");
        fs::create_dir_all(&src).unwrap();
        git(&["init", "-q", "-b", "main"], &src);
        fs::write(src.join("PKGBUILD"), "pkgname=foo\n").unwrap();
        fs::write(src.join("README"), "hello\n").unwrap();
        git(&["add", "-A"], &src);
        git(&["commit", "-q", "-m", "init"], &src);
        git(
            &[
                "clone",
                "-q",
                "--bare",
                src.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
            dir,
        );
        let repo = gix::open(&bare).unwrap();
        (bare, repo)
    }

    #[test]
    fn add_creates_linked_worktree_that_git_recognises() {
        let td = TempDir::new().unwrap();
        let (bare, repo) = make_bare(td.path());
        let mirror = MirrorRepo {
            path: bare.clone(),
            repo,
        };
        let dest = td.path().join("pkgs/foo");

        let wt = add_or_reset(&mirror, &branch("main"), &dest).unwrap();

        // Tracked files materialized.
        assert!(dest.join("PKGBUILD").exists());
        assert!(dest.join("README").exists());
        // git wired both sides: pointer in worktree, admin under bare.
        assert!(dest.join(".git").is_file());
        assert!(bare.join("worktrees/foo/HEAD").exists());

        // Native git sees this worktree from the bare side.
        let out = Command::new("git")
            .args([
                "-C",
                bare.to_str().unwrap(),
                "worktree",
                "list",
                "--porcelain",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&format!("worktree {}", dest.display())),
            "got:\n{stdout}"
        );

        // OID round-trip: the admin HEAD must point at the same commit we
        // peeled via gix.
        let head_in_admin = fs::read_to_string(bare.join("worktrees/foo/HEAD")).unwrap();
        assert!(head_in_admin
            .trim_end()
            .starts_with(&wt.head_oid.to_string()));
    }

    #[test]
    fn reset_preserves_untracked_makepkg_scratch() {
        // The whole reason we switched to `git reset --hard`: rebuilds must
        // NOT wipe `pkg/`, `src/`, `src-cache/`, or cached `.pkg.tar.zst`.
        // Otherwise the idempotency cache in build_one is dead code and we
        // re-download sources on every run.
        let td = TempDir::new().unwrap();
        let (bare, repo) = make_bare(td.path());
        let mirror = MirrorRepo { path: bare, repo };
        let dest = td.path().join("pkgs/foo");

        add_or_reset(&mirror, &branch("main"), &dest).unwrap();
        // Simulate a previous makepkg run leaving scratch behind.
        fs::create_dir(dest.join("src")).unwrap();
        fs::write(dest.join("src/downloaded.tar.gz"), b"hi").unwrap();
        fs::write(dest.join("foo-1-1-x86_64.pkg.tar.zst"), b"pkg").unwrap();

        add_or_reset(&mirror, &branch("main"), &dest).unwrap();

        assert!(
            dest.join("src/downloaded.tar.gz").exists(),
            "untracked makepkg src dir must survive reset",
        );
        assert!(
            dest.join("foo-1-1-x86_64.pkg.tar.zst").exists(),
            "cached .pkg.tar.zst must survive — idempotency shortcut depends on it",
        );
        // Tracked files still correct after the reset.
        assert!(dest.join("PKGBUILD").exists());
    }

    #[test]
    fn reset_undoes_modifications_to_tracked_files() {
        // Untracked content is preserved, but TRACKED files must snap back
        // to the branch tip — that's the whole job of `reset --hard`. If a
        // user edited PKGBUILD locally between builds, the second build
        // should re-materialize the index's version.
        let td = TempDir::new().unwrap();
        let (bare, repo) = make_bare(td.path());
        let mirror = MirrorRepo { path: bare, repo };
        let dest = td.path().join("pkgs/foo");

        add_or_reset(&mirror, &branch("main"), &dest).unwrap();
        fs::write(dest.join("PKGBUILD"), b"tampered\n").unwrap();
        add_or_reset(&mirror, &branch("main"), &dest).unwrap();
        assert_eq!(
            fs::read_to_string(dest.join("PKGBUILD")).unwrap(),
            "pkgname=foo\n",
            "reset --hard must restore tracked files to branch tip",
        );
    }

    #[test]
    fn prune_handles_fakeroot_restrictive_perms() {
        // Regression: bisq's `pkg/` subtree contains directories with mode
        // 0111 from fakeroot. `std::fs::remove_dir_all` can't traverse those
        // and bombs with EACCES. We now delegate to `git worktree remove
        // --force` → coreutils `rm -rf`, which chmod-then-removes.
        let td = TempDir::new().unwrap();
        let (bare, repo) = make_bare(td.path());
        let mirror = MirrorRepo { path: bare, repo };
        let dest = td.path().join("pkgs/foo");

        add_or_reset(&mirror, &branch("main"), &dest).unwrap();
        // Mimic the failure mode: a directory we can't traverse.
        let trap = dest.join("pkg/restricted");
        fs::create_dir_all(&trap).unwrap();
        fs::write(trap.join("inner"), b"data").unwrap();
        let mut perms = fs::metadata(&trap).unwrap().permissions();
        perms.set_mode(0o111);
        fs::set_permissions(&trap, perms).unwrap();

        prune(&mirror, "foo", &dest).expect("prune must handle restrictive perms");
        assert!(!dest.exists(), "prune must remove dest tree entirely");
    }

    #[test]
    fn prune_propagates_git_failure() {
        // `prune` runs `git -C <mirror.path> worktree prune`. If that fails
        // (mirror path doesn't exist, isn't a git repo, ...) the function must
        // bubble the error up rather than swallowing it — otherwise callers
        // think the postcondition ("admin entry removed") was met when it
        // wasn't, and the next `worktree add` trips on a stale entry with no
        // breadcrumb back to the actual failure.
        let td = TempDir::new().unwrap();
        // Real gix repo so MirrorRepo is constructible, but the `path` field
        // points at a non-existent directory so the shelled-out git call fails.
        let (_bare, repo) = make_bare(td.path());
        let mirror = MirrorRepo {
            path: td.path().join("does-not-exist"),
            repo,
        };
        let dest = td.path().join("pkgs/foo"); // also non-existent → force_remove skipped

        let err = prune(&mirror, "foo", &dest).expect_err("must surface git failure");
        // We don't care which exact phrasing git uses across versions, only
        // that the error came from the git shell-out (Error::Gix) and names
        // the operation.
        let msg = format!("{err}");
        assert!(msg.contains("worktree prune"), "unexpected error: {msg}");
    }
}
