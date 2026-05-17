//! Linked git worktrees pointing back at the bare mirror.
//!
//! gix 0.83 has no `git worktree add` API at any level, but the on-disk
//! format has been stable since git 2.5: a tiny admin directory under the
//! bare repo plus a `.git` pointer file in the worktree. We write those
//! by hand so `cd ~/.local/state/gitaur/pkgs/<pkgbase> && git log` works
//! natively, without pulling in a git binary dependency.
//!
//! Layout produced:
//!
//! ```text
//! <bare>/worktrees/<pkgbase>/
//!     HEAD         "<commit-oid>\n"   (detached at the branch tip)
//!     commondir    "../..\n"
//!     gitdir       "<abs path to pkgs/<pkgbase>/.git>\n"
//! pkgs/<pkgbase>/
//!     .git         "gitdir: <abs path to bare/worktrees/<pkgbase>>\n"
//!     PKGBUILD, .SRCINFO, ...   (materialized from the branch's tree)
//! ```

use crate::error::{Error, Result};
use crate::mirror::MirrorRepo;
use gix::bstr::BStr;
use gix::object::tree::{EntryKind, EntryMode};
use gix::ObjectId;
use std::path::{Path, PathBuf};
use tracing::{debug, instrument};

/// One pkgbase's build directory plus the commit it was materialized from.
pub struct Worktree {
    /// Materialized files live here.
    pub path: PathBuf,
    /// Commit OID we checked out, for state.db recording and diff-on-update.
    pub head_oid: ObjectId,
}

/// Create or reset a linked worktree for `branch` at `dest`.
///
/// If a previous worktree exists at `dest`, it (and its admin dir under the
/// bare repo) is removed and re-created, then files are re-materialized from
/// the current branch tip.
#[instrument(skip(mirror))]
pub fn add_or_reset(mirror: &MirrorRepo, branch: &str, dest: &Path) -> Result<Worktree> {
    let refname = format!("refs/heads/{branch}");
    let head_oid = {
        let mut r = mirror
            .repo
            .find_reference(refname.as_str())
            .map_err(|e| Error::Gix(format!("find_reference {refname}: {e}")))?;
        let commit = r
            .peel_to_id()
            .map_err(|e| Error::Gix(format!("peel {refname}: {e}")))?;
        commit.detach()
    };
    let tree_oid = mirror
        .repo
        .find_commit(head_oid)
        .map_err(|e| Error::Gix(format!("find_commit {head_oid}: {e}")))?
        .tree_id()
        .map_err(|e| Error::Gix(format!("tree_id {head_oid}: {e}")))?
        .detach();

    // Clean slate. State.db is the source of truth for "last built commit";
    // the on-disk worktree is just scratch.
    let admin_dir = bare_worktree_admin(&mirror.path, branch);
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    if admin_dir.exists() {
        std::fs::remove_dir_all(&admin_dir)?;
    }
    std::fs::create_dir_all(dest)?;
    std::fs::create_dir_all(&admin_dir)?;

    write_admin_files(&admin_dir, dest, head_oid)?;
    write_gitlink(dest, &admin_dir)?;
    materialize_tree(mirror, tree_oid, dest)?;

    debug!(branch, %head_oid, %tree_oid, admin = %admin_dir.display(), "linked worktree ready");
    Ok(Worktree {
        path: dest.to_path_buf(),
        head_oid,
    })
}

/// Remove a worktree's files and admin directory. Idempotent.
pub fn prune(mirror: &MirrorRepo, branch: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        std::fs::remove_dir_all(dest)?;
    }
    let admin = bare_worktree_admin(&mirror.path, branch);
    if admin.exists() {
        std::fs::remove_dir_all(&admin)?;
    }
    Ok(())
}

fn bare_worktree_admin(bare: &Path, branch: &str) -> PathBuf {
    bare.join("worktrees").join(branch)
}

/// Write `HEAD`, `commondir`, `gitdir` inside the admin directory.
fn write_admin_files(admin: &Path, worktree_dir: &Path, head_oid: ObjectId) -> Result<()> {
    // Detached HEAD at the branch tip — keeps things simple and reset-safe.
    std::fs::write(admin.join("HEAD"), format!("{head_oid}\n"))?;
    // Relative path from <bare>/worktrees/<name>/ back to <bare>.
    std::fs::write(admin.join("commondir"), "../..\n")?;
    // Absolute path to the worktree's `.git` *file* (not the worktree dir).
    let gitfile = worktree_dir
        .canonicalize()
        .unwrap_or_else(|_| worktree_dir.to_path_buf())
        .join(".git");
    std::fs::write(admin.join("gitdir"), format!("{}\n", gitfile.display()))?;
    Ok(())
}

/// Write the `.git` pointer file inside the worktree.
fn write_gitlink(worktree_dir: &Path, admin: &Path) -> Result<()> {
    let abs_admin = admin.canonicalize().unwrap_or_else(|_| admin.to_path_buf());
    std::fs::write(
        worktree_dir.join(".git"),
        format!("gitdir: {}\n", abs_admin.display()),
    )?;
    Ok(())
}

/// Recursively write a tree's contents to `dest`.
fn materialize_tree(mirror: &MirrorRepo, tree_oid: ObjectId, dest: &Path) -> Result<()> {
    let tree = mirror
        .repo
        .find_tree(tree_oid)
        .map_err(|e| Error::Gix(format!("find_tree {tree_oid}: {e}")))?;
    for entry in tree.iter() {
        let entry = entry.map_err(|e| Error::Gix(format!("iter tree: {e}")))?;
        write_entry(mirror, entry.filename(), entry.oid(), entry.mode(), dest)?;
    }
    Ok(())
}

fn write_entry(
    mirror: &MirrorRepo,
    name: &BStr,
    oid: &gix::oid,
    mode: EntryMode,
    parent: &Path,
) -> Result<()> {
    let name_str = std::str::from_utf8(name.as_ref())
        .map_err(|e| Error::Gix(format!("non-utf8 path: {e}")))?;
    if name_str == ".git" {
        // Defensive: a tree should never contain `.git`, but if one did it
        // would clobber our pointer file. Skip with a warning.
        debug!(path = %parent.join(name_str).display(), "skipping in-tree .git");
        return Ok(());
    }
    let child = parent.join(name_str);

    let obj = mirror
        .repo
        .find_object(oid.to_owned())
        .map_err(|e| Error::Gix(format!("find_object {oid}: {e}")))?;

    match mode.kind() {
        EntryKind::Tree => {
            std::fs::create_dir_all(&child)?;
            materialize_tree(mirror, oid.to_owned(), &child)?;
        }
        EntryKind::Blob => {
            std::fs::write(&child, obj.data.as_slice())?;
        }
        EntryKind::BlobExecutable => {
            std::fs::write(&child, obj.data.as_slice())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mut perm = std::fs::metadata(&child)?.permissions();
                perm.set_mode(0o755);
                std::fs::set_permissions(&child, perm)?;
            }
        }
        EntryKind::Link => {
            let target = std::str::from_utf8(obj.data.as_slice())
                .map_err(|e| Error::Gix(format!("symlink target utf8: {e}")))?;
            let _ = std::fs::remove_file(&child);
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &child)?;
        }
        EntryKind::Commit => {
            // Submodule pointer; AUR PKGBUILDs never use them.
            debug!(path = %child.display(), "skipping submodule entry");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::git;
    use std::process::Command;
    use tempfile::TempDir;

    /// Build a tiny bare repo via the system `git` so the test exercises the
    /// real on-disk format we're about to wire up by hand.
    fn make_bare(dir: &Path) -> (PathBuf, gix::Repository) {
        let src = dir.join("src");
        let bare = dir.join("bare");
        std::fs::create_dir_all(&src).unwrap();
        git(&["init", "-q", "-b", "main"], &src);
        std::fs::write(src.join("PKGBUILD"), "pkgname=foo\n").unwrap();
        std::fs::write(src.join("README"), "hello\n").unwrap();
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

        let wt = add_or_reset(&mirror, "main", &dest).unwrap();

        // Files materialized.
        assert!(dest.join("PKGBUILD").exists());
        assert!(dest.join("README").exists());
        // Linkage written.
        assert!(dest.join(".git").is_file());
        assert!(bare.join("worktrees/main/HEAD").exists());
        assert!(bare.join("worktrees/main/commondir").exists());
        assert!(bare.join("worktrees/main/gitdir").exists());

        // Native git accepts it.
        let status = Command::new("git")
            .args(["-C", dest.to_str().unwrap(), "status", "--porcelain"])
            .status()
            .unwrap();
        assert!(status.success(), "git status must accept our linkage");

        // Native git sees this worktree from the bare side too.
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

        // OID round-trip.
        let head_in_admin = std::fs::read_to_string(bare.join("worktrees/main/HEAD")).unwrap();
        assert!(head_in_admin
            .trim_end()
            .starts_with(&wt.head_oid.to_string()));
    }

    #[test]
    fn reset_replaces_contents() {
        let td = TempDir::new().unwrap();
        let (bare, repo) = make_bare(td.path());
        let mirror = MirrorRepo { path: bare, repo };
        let dest = td.path().join("pkgs/foo");
        add_or_reset(&mirror, "main", &dest).unwrap();
        std::fs::write(dest.join("scratch"), "junk").unwrap();
        let _wt = add_or_reset(&mirror, "main", &dest).unwrap();
        assert!(!dest.join("scratch").exists(), "reset must wipe local mods");
        assert!(dest.join("PKGBUILD").exists());
    }
}
