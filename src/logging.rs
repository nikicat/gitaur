//! Tracing subscriber setup. A console layer (env-filter, default `warn`) plus
//! a per-invocation file layer at `debug` written to `state_dir()/logs/`. Old
//! log files are pruned to [`KEEP_LOGS`] most recent on every startup.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter, Layer};

use crate::paths;

const KEEP_LOGS: usize = 10;

/// File-layer default filter. Baseline is `debug` so gix-progress state
/// changes (`set_name`, `add_child`, `message`) land in the log, but the very
/// chatty per-percent `trace!` events do not. Per-crate overrides silence the
/// HTTP-plumbing layers (h2 frame-by-frame, hyper connection pool, rustls
/// platform verifier, reqwest connect) which otherwise drown gitaur's own
/// events ~5:1 during a single fetch.
const FILE_LOG_FILTER: &str =
    "debug,h2=info,hyper=info,hyper_util=info,reqwest=info,rustls=info,rustls_platform_verifier=info";

/// Initialize tracing. Best-effort: console logging always works; if the log
/// file can't be created we print a warning to stderr and continue without
/// file logging. Returns the log file path when file logging is active.
pub fn init() -> Option<PathBuf> {
    let console_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    // `fmt::layer()` defaults to stdout, which competes with subprocess
    // stdout (makepkg, pacman -U). Pin to stderr so log lines interleave
    // cleanly with `ui::{step,note,…}` (which all use eprintln) and don't
    // pollute callers that capture gitaur's stdout.
    let console_layer = fmt::layer()
        .with_target(false)
        .with_writer(std::io::stderr)
        .with_filter(console_filter);

    let (file_layer, log_path) = match open_log_file() {
        Ok((file, path)) => {
            let layer = fmt::layer()
                .with_ansi(false)
                .with_target(true)
                .with_timer(JiffTimer)
                .with_writer(Mutex::new(file))
                .with_filter(EnvFilter::new(FILE_LOG_FILTER));
            (Some(layer), Some(path))
        }
        Err(e) => {
            eprintln!("gitaur: file logging disabled: {e}");
            (None, None)
        }
    };

    tracing_subscriber::registry()
        .with(console_layer)
        .with(file_layer)
        .init();

    if let Some(path) = &log_path {
        tracing::debug!(path = %path.display(), "execution log opened");
        if let Err(e) = prune_old_logs_in(&paths::logs_dir(), KEEP_LOGS) {
            tracing::warn!(error = %e, "failed to prune old gitaur logs");
        }
    }

    log_path
}

fn open_log_file() -> std::io::Result<(File, PathBuf)> {
    paths::ensure_logs_dir()?;
    let path = new_log_file_path();
    let file = File::create(&path)?;
    Ok((file, path))
}

fn new_log_file_path() -> PathBuf {
    let stamp = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S");
    let pid = std::process::id();
    paths::logs_dir().join(format!("gitaur-{stamp}-{pid}.log"))
}

fn is_log_file(name: &std::ffi::OsStr) -> bool {
    let Some(s) = name.to_str() else { return false };
    s.starts_with("gitaur-")
        && Path::new(s)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("log"))
}

fn prune_old_logs_in(dir: &Path, keep: usize) -> std::io::Result<()> {
    let mut entries: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter(|e| is_log_file(&e.file_name()))
        .filter_map(|e| {
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, e.path()))
        })
        .collect();
    entries.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    for (_, path) in entries.into_iter().skip(keep) {
        let _ = std::fs::remove_file(&path);
    }
    Ok(())
}

struct JiffTimer;

impl FormatTime for JiffTimer {
    fn format_time(&self, w: &mut fmt::format::Writer<'_>) -> std::fmt::Result {
        write!(
            w,
            "{}",
            jiff::Zoned::now().strftime("%Y-%m-%dT%H:%M:%S%.3f%:z")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn touch_with_mtime(path: &Path, mtime: SystemTime) {
        let f = File::create(path).unwrap();
        f.set_modified(mtime).unwrap();
    }

    #[test]
    fn prune_keeps_n_newest_log_files() {
        let dir = tempdir().unwrap();
        let now = SystemTime::now();
        let mut all = Vec::new();
        for i in 0u64..15 {
            let p = dir.path().join(format!("gitaur-{i:02}.log"));
            // Older files come first: i=0 is oldest, i=14 is newest.
            touch_with_mtime(&p, now - Duration::from_secs(60 * (15 - i)));
            all.push(p);
        }
        // A non-log file should be ignored entirely.
        fs::write(dir.path().join("not-a-log.txt"), "").unwrap();
        // A non-gitaur log should also be ignored.
        touch_with_mtime(&dir.path().join("other-1.log"), now);

        prune_old_logs_in(dir.path(), 10).unwrap();

        for (i, p) in all.iter().enumerate() {
            let kept = p.exists();
            if i >= 5 {
                assert!(kept, "expected to keep {}", p.display());
            } else {
                assert!(!kept, "expected to prune {}", p.display());
            }
        }
        assert!(dir.path().join("not-a-log.txt").exists());
        assert!(dir.path().join("other-1.log").exists());
    }

    #[test]
    fn prune_is_noop_when_under_limit() {
        let dir = tempdir().unwrap();
        let now = SystemTime::now();
        for i in 0u64..3 {
            touch_with_mtime(
                &dir.path().join(format!("gitaur-{i}.log")),
                now - Duration::from_secs(i),
            );
        }
        prune_old_logs_in(dir.path(), 10).unwrap();
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
    }
}
