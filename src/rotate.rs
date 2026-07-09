//! Creation + retention for aurox's per-run files (logs, traces).
//!
//! [`RotationPolicy`] is the contract a family of per-run artifacts implements:
//! where its files live, their extension, and how many to keep. The provided
//! methods then create this run's file ([`create`](RotationPolicy::create)) and
//! cap the directory on startup ([`prune`](RotationPolicy::prune)) via the
//! filename-agnostic [`prune_keeping_newest`] mechanism. Concrete policies (the
//! `Logs`/`Traces` types in [`crate::logging`]) supply only the specifics.

use std::ffi::OsStr;
use std::fs::File;
use std::path::{Path, PathBuf};

/// Stem prefix shared by every per-run file. Lets [`RotationPolicy::owns`]
/// avoid touching unrelated files a user might drop in the directory.
const PREFIX: &str = "aurox-";

/// `aurox-<timestamp>-<pid>` — the filename stem for this run. Computed once
/// per run and handed to every policy's [`create`](RotationPolicy::create) so a
/// run's log and trace share one stem and are trivial to correlate.
pub(crate) fn run_basename() -> String {
    let stamp = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S");
    let pid = std::process::id();
    format!("{PREFIX}{stamp}-{pid}")
}

/// One family of per-run artifacts that aurox creates each run and caps on
/// startup. Implementors describe the family; the provided methods do the work.
pub(crate) trait RotationPolicy {
    /// Directory holding this family's files. Resolved at call time (not
    /// stored) so a `state_dir()` override in tests is honored. Doubles as the
    /// family's identity in diagnostics.
    fn dir(&self) -> PathBuf;

    /// Filename extension for this family, without the dot (e.g. "log").
    fn ext(&self) -> &'static str;

    /// How many of the newest files to retain.
    fn keep(&self) -> usize;

    /// Create this run's file at `<dir>/<basename>.<ext>`, making the directory
    /// if missing. `basename` comes from [`run_basename`].
    fn create(&self, basename: &str) -> std::io::Result<(File, PathBuf)> {
        let dir = self.dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{basename}.{}", self.ext()));
        let file = File::create(&path)?;
        Ok((file, path))
    }

    /// Whether `name` is a file this policy owns (and may prune): the shared
    /// [`PREFIX`] stem plus this family's [`ext`](Self::ext).
    fn owns(&self, name: &OsStr) -> bool {
        let Some(s) = name.to_str() else { return false };
        s.starts_with(PREFIX)
            && Path::new(s)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case(self.ext()))
    }

    /// Keep the newest `keep()` owned files and unlink the rest. Owns the whole
    /// decision: a directory that doesn't exist yet is a no-op (nothing has
    /// been written to rotate), any other I/O error is reported at `warn`.
    /// Fire-and-forget — callers prune on startup and don't act on failure.
    fn prune(&self) {
        let dir = self.dir();
        match prune_keeping_newest(&dir, self.keep(), |n| self.owns(n)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "failed to prune old files"),
        }
    }
}

/// Keep the `keep` most-recently-modified files in `dir` whose name satisfies
/// `matches`; unlink the rest. Best-effort: a failed unlink (e.g. a directory
/// matched the predicate) is logged at `debug` and the sweep continues.
fn prune_keeping_newest(
    dir: &Path,
    keep: usize,
    matches: impl Fn(&OsStr) -> bool,
) -> std::io::Result<()> {
    let mut entries: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter(|e| matches(&e.file_name()))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    entries.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    for (_, path) in entries.into_iter().skip(keep) {
        // Safe to `tracing::debug!` here: the caller's writers point at this
        // run's files, which are different inodes from any of the (older)
        // files being unlinked here. No re-entry.
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::debug!(path = %path.display(), error = %e, "failed to prune old file");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};
    use tempfile::{TempDir, tempdir};

    /// A policy rooted at a tempdir, for exercising the provided methods
    /// (`create`, `owns`, `prune`) end to end without touching `state_dir()`.
    struct TestPolicy {
        dir: PathBuf,
        keep: usize,
    }

    impl RotationPolicy for TestPolicy {
        fn dir(&self) -> PathBuf {
            self.dir.clone()
        }
        fn ext(&self) -> &'static str {
            "log"
        }
        fn keep(&self) -> usize {
            self.keep
        }
    }

    fn policy(keep: usize) -> (TempDir, TestPolicy) {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("nested"); // exercises create_dir_all
        let p = TestPolicy { dir, keep };
        (tmp, p)
    }

    fn touch_with_mtime(path: &Path, mtime: SystemTime) {
        let f = File::create(path).unwrap();
        f.set_modified(mtime).unwrap();
    }

    #[test]
    fn create_makes_dir_and_extension() {
        let (_tmp, p) = policy(10);
        let (_file, path) = p.create("aurox-20260526-120000-42").unwrap();
        assert!(path.exists());
        assert_eq!(path.extension().unwrap(), "log");
        assert!(path.starts_with(p.dir()), "file lands in the policy dir");
    }

    #[test]
    fn owns_matches_stem_and_extension() {
        let (_tmp, p) = policy(10);
        assert!(p.owns(OsStr::new("aurox-x.log")));
        assert!(!p.owns(OsStr::new("aurox-x.json"))); // wrong extension
        assert!(!p.owns(OsStr::new("other.log"))); // missing stem
    }

    #[test]
    fn prune_on_missing_dir_is_silent_noop() {
        let (_tmp, p) = policy(10);
        // `dir` (…/nested) was never created; prune must not warn or panic.
        p.prune();
        assert!(!p.dir().exists());
    }

    #[test]
    fn keeps_n_newest_owned_files() {
        let (_tmp, p) = policy(10);
        std::fs::create_dir_all(p.dir()).unwrap();
        let now = SystemTime::now();
        let mut all = Vec::new();
        for i in 0u64..15 {
            let path = p.dir().join(format!("aurox-{i:02}.log"));
            // Older files come first: i=0 is oldest, i=14 is newest.
            touch_with_mtime(&path, now - Duration::from_secs(60 * (15 - i)));
            all.push(path);
        }
        // Unowned files (wrong stem / extension) must survive.
        std::fs::write(p.dir().join("not-a-log.txt"), "").unwrap();
        touch_with_mtime(&p.dir().join("other-1.log"), now);

        p.prune();

        for (i, path) in all.iter().enumerate() {
            let kept = path.exists();
            if i >= 5 {
                assert!(kept, "expected to keep {}", path.display());
            } else {
                assert!(!kept, "expected to prune {}", path.display());
            }
        }
        assert!(p.dir().join("not-a-log.txt").exists());
        assert!(p.dir().join("other-1.log").exists());
    }

    #[test]
    fn prune_continues_after_one_unlink_failure() {
        // Plant a *directory* whose name `owns()` matches, so it lands in the
        // prune-candidate list; the `remove_file()` then fails with EISDIR.
        // The diagnostic branch should log and keep going so the other targets
        // still get removed.
        let (_tmp, p) = policy(10);
        std::fs::create_dir_all(p.dir()).unwrap();
        let now = SystemTime::now();
        let mut files = Vec::new();
        for i in 1u64..15 {
            let path = p.dir().join(format!("aurox-{i:02}.log"));
            // i=1 is oldest file, i=14 is newest.
            touch_with_mtime(&path, now - Duration::from_secs(60 * (15 - i)));
            files.push((i, path));
        }
        let trap = p.dir().join("aurox-00.log");
        std::fs::create_dir(&trap).unwrap();
        // Trap is older than any file so it ends up in the prune tail.
        File::open(&trap)
            .unwrap()
            .set_modified(now - Duration::from_mins(100))
            .unwrap();

        p.prune();

        // Trap survived its unlink attempt.
        assert!(trap.is_dir(), "EISDIR trap should still exist");
        // Five prune slots, the trap took one; the four next-oldest files
        // (i=1..=4) should have been removed; the 10 newest kept.
        for (i, path) in &files {
            let kept = path.exists();
            if *i <= 4 {
                assert!(!kept, "expected to prune {}", path.display());
            } else {
                assert!(kept, "expected to keep {}", path.display());
            }
        }
    }
}
