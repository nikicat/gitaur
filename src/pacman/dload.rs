//! aurox's own downloader for official-repo files — libalpm's external
//! "fetch callback", implemented with curl.
//!
//! Exists for one reason: **interruptibility**. libalpm's internal curl
//! downloader cannot be aborted from outside — its interrupt flag is a
//! file-private static nothing sets on SIGINT (pacman itself just `_Exit`s the
//! whole process on Ctrl+C), so an `alpm_db_update` mid-download runs to
//! completion no matter what the caller wants. Registering a fetch callback
//! ([`crate::pacman::sync`] does, via `Alpm::set_fetch_cb`) replaces that
//! downloader with this one, whose curl progress meter watches a shared
//! interrupt flag — curl fires it at least once a second even on a silent
//! socket, so a Ctrl+C aborts the transfer within a beat.
//!
//! Behaviour mirrors what libalpm's internal downloader does for a sync DB, so
//! swapping the engine changes nothing else: `If-Modified-Since` from the
//! existing file's mtime (unless `force`), the server's `Last-Modified` stamped
//! onto the downloaded file (that mtime is what the *next* refresh's
//! `If-Modified-Since` compares against), a `.part` staging file renamed into
//! place only on success, and libalpm's own timeouts (10 s connect, abort under
//! 1 byte/s for 10 s). One code path for every URL scheme curl speaks — the
//! container suite's `file://` repos included.

use crate::error::{Error, Result};
use crate::units::{ByteSize, UnixTime};
use curl::easy::{Easy, TimeCondition};
use std::fs::{self, File, FileTimes};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};
use tracing::debug;

/// How one [`FetchSpec::run`] ended — the two success shapes libalpm's fetch
/// callback distinguishes (`0` = downloaded, `1` = already up to date).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchOutcome {
    /// A new copy of the file was downloaded into the destination directory.
    Downloaded,
    /// The server answered "not modified since your copy's mtime" — nothing
    /// was transferred and the existing file is untouched.
    Unchanged,
}

/// One beat of a running transfer, as reported to [`FetchSpec::run`]'s
/// progress callback.
#[derive(Debug, Clone, Copy)]
pub struct DownloadProgress {
    /// Bytes received so far.
    pub downloaded: ByteSize,
    /// The transfer's full size — `None` until the server announces one
    /// (curl reports 0 while the length is still unknown).
    pub total: Option<ByteSize>,
}

/// Where one fetch lands on disk: bytes stream into the `part` staging file,
/// which is renamed to `dest` only once the download completed — the final
/// name never holds a partial file.
struct Staging {
    /// Final path (`<dest_dir>/<url basename>`).
    dest: PathBuf,
    /// In-flight staging path (`<dest>.part`), removed on failure/unchanged.
    part: PathBuf,
}

/// One file to fetch on libalpm's behalf: the URL it composed, where it
/// expects the file to land, and how to bail out.
#[derive(Debug)]
pub struct FetchSpec<'a> {
    /// Full remote URL (`<server>/<filename>`), as libalpm composed it.
    pub url: &'a str,
    /// Directory the file lands in — libalpm's `localpath` (the sync dir).
    pub dest_dir: &'a Path,
    /// Re-download unconditionally (`-Syy`): skip the `If-Modified-Since`
    /// shortcut so a fresh copy always lands.
    pub force: bool,
    /// Cooperative abort, flipped by [`crate::interrupt::cancel_on_sigint`]'s
    /// watcher on Ctrl+C and checked from curl's progress meter.
    pub interrupt: &'a AtomicBool,
}

impl FetchSpec<'_> {
    /// Download the file, reporting a [`DownloadProgress`] beat to `progress`
    /// as the transfer advances.
    ///
    /// On interrupt the transfer aborts within curl's progress-meter beat
    /// (~1 s worst case, immediate while data flows) and this returns
    /// [`Error::Interrupted`]; the `.part` staging file is removed, so a
    /// re-run starts clean.
    pub fn run(&self, mut progress: impl FnMut(DownloadProgress)) -> Result<FetchOutcome> {
        let Staging { dest, part } = self.staging()?;
        let mut handle = Easy::new();
        handle.url(self.url)?;
        handle.useragent(concat!("aurox/", env!("CARGO_PKG_VERSION")))?;
        handle.follow_location(true)?;
        handle.max_redirections(10)?;
        // HTTP >= 400 becomes a curl error instead of a downloaded error page.
        handle.fail_on_error(true)?;
        // libalpm's own transfer guards: 10 s to connect, and a transfer
        // sitting under 1 byte/s for 10 s is dead. The interrupt flag stays
        // the *fast* exit; these only reap peers that went silent on their own.
        handle.connect_timeout(Duration::from_secs(10))?;
        handle.low_speed_limit(1)?;
        handle.low_speed_time(Duration::from_secs(10))?;
        // Ask curl for the remote's Last-Modified so the downloaded file can
        // carry it as its mtime (what the next If-Modified-Since compares).
        handle.fetch_filetime(true)?;
        if let Some(since) = self.if_modified_since(&dest) {
            handle.time_condition(TimeCondition::IfModifiedSince)?;
            handle.time_value(since.seconds())?;
        }
        handle.progress(true)?;

        // Stage into `.part`: the real filename only ever holds a complete
        // download. Created eagerly so a 200-with-empty-body still lands a
        // (legal, zero-byte) file.
        let mut out = File::create(&part)
            .map_err(|e| Error::other(format!("create {}: {e}", part.display())))?;
        let mut write_err: Option<std::io::Error> = None;
        let transferred = {
            let mut transfer = handle.transfer();
            transfer.write_function(|data| {
                match out.write_all(data) {
                    Ok(()) => Ok(data.len()),
                    Err(e) => {
                        // A short write aborts the transfer; carry the real
                        // io error out instead of curl's generic "write error".
                        write_err = Some(e);
                        Ok(0)
                    }
                }
            })?;
            transfer.progress_function(|dltotal, dlnow, _, _| {
                if self.interrupt.load(Ordering::SeqCst) {
                    return false;
                }
                let total = bytes(dltotal);
                progress(DownloadProgress {
                    downloaded: ByteSize::new(bytes(dlnow)),
                    total: (total > 0).then(|| ByteSize::new(total)),
                });
                true
            })?;
            transfer.perform()
        };

        if let Err(e) = transferred {
            drop(out);
            fs::remove_file(&part).ok();
            // The flag is authoritative: whatever error the abort surfaced as,
            // the story is "the user interrupted".
            if self.interrupt.load(Ordering::SeqCst) || e.is_aborted_by_callback() {
                debug!(url = self.url, "download interrupted");
                return Err(Error::Interrupted);
            }
            if let Some(io) = write_err {
                return Err(Error::other(format!(
                    "download {}: write {}: {io}",
                    self.url,
                    part.display()
                )));
            }
            return Err(Error::other(format!("download {}: {e}", self.url)));
        }

        // 304-equivalent: the time condition wasn't met, so no body was sent
        // and the existing file stands.
        if handle.time_condition_unmet()? {
            drop(out);
            fs::remove_file(&part).ok();
            debug!(url = self.url, "unchanged since last download");
            return Ok(FetchOutcome::Unchanged);
        }

        out.flush()
            .map_err(|e| Error::other(format!("flush {}: {e}", part.display())))?;
        // Best-effort mtime stamp — a server without Last-Modified (curl
        // reports -1, filtered by `known()` inside `system_time`) just means
        // the next refresh re-downloads instead of 304ing.
        if let Ok(Some(remote_time)) = handle.filetime()
            && let Some(stamp) = UnixTime::new(remote_time).system_time()
        {
            out.set_times(FileTimes::new().set_modified(stamp)).ok();
        }
        drop(out);
        fs::rename(&part, &dest).map_err(|e| {
            Error::other(format!(
                "rename {} -> {}: {e}",
                part.display(),
                dest.display()
            ))
        })?;
        debug!(url = self.url, dest = %dest.display(), "downloaded");
        Ok(FetchOutcome::Downloaded)
    }

    /// The [`Staging`] paths for this URL's basename.
    fn staging(&self) -> Result<Staging> {
        let name = self
            .url
            .rsplit('/')
            .next()
            .filter(|n| !n.is_empty())
            .ok_or_else(|| Error::other(format!("download {}: no filename in URL", self.url)))?;
        Ok(Staging {
            dest: self.dest_dir.join(name),
            part: self.dest_dir.join(format!("{name}.part")),
        })
    }

    /// The `If-Modified-Since` timestamp to send: the existing file's mtime,
    /// unless `force` (or there is no usable existing copy — libalpm likewise
    /// refuses to 304 against an empty file).
    fn if_modified_since(&self, dest: &Path) -> Option<UnixTime> {
        if self.force {
            return None;
        }
        let meta = fs::metadata(dest).ok().filter(|m| m.len() > 0)?;
        let secs = meta
            .modified()
            .ok()?
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs();
        Some(UnixTime::new(i64::try_from(secs).ok()?))
    }
}

/// Curl reports byte counts as `f64`; clamp the nonsensical shapes (negative,
/// NaN) to 0 for the progress display.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn bytes(count: f64) -> u64 {
    if count.is_finite() && count > 0.0 {
        count as u64
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::{FetchOutcome, FetchSpec};
    use crate::context;
    use crate::error::Error;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant, SystemTime};
    use tempfile::TempDir;

    /// Serve exactly one HTTP request on a fresh localhost port, answering
    /// with `response` verbatim. Returns the port and a channel yielding the
    /// request's header block (for asserting what the client sent).
    fn one_shot_server(
        s: &context::Scope<'_, '_>,
        response: &'static [u8],
    ) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        s.spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(&stream);
            let mut headers = String::new();
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                if line == "\r\n" || line == "\n" {
                    break;
                }
                headers.push_str(&line);
                line.clear();
            }
            tx.send(headers).unwrap();
            (&stream).write_all(response).unwrap();
        });
        (port, rx)
    }

    #[test]
    fn downloads_and_stamps_the_servers_mtime() {
        let dir = TempDir::new().unwrap();
        let interrupt = AtomicBool::new(false);
        context::scope(|s| {
            let (port, headers) = one_shot_server(
                s,
                b"HTTP/1.1 200 OK\r\n\
                  Last-Modified: Wed, 01 Jan 2020 00:00:00 GMT\r\n\
                  Content-Length: 6\r\n\
                  Connection: close\r\n\r\n\
                  DBDATA",
            );
            let url = format!("http://127.0.0.1:{port}/core.db");
            let spec = FetchSpec {
                url: &url,
                dest_dir: dir.path(),
                force: false,
                interrupt: &interrupt,
            };
            let mut seen = 0;
            let out = spec.run(|p| seen = p.downloaded.bytes()).unwrap();
            assert_eq!(out, FetchOutcome::Downloaded);
            assert_eq!(seen, 6, "progress saw the whole body");
            // No existing copy — the request must not claim to have one.
            assert!(!headers.recv().unwrap().contains("If-Modified-Since"));
        });

        let dest = dir.path().join("core.db");
        assert_eq!(fs::read(&dest).unwrap(), b"DBDATA");
        assert!(!dir.path().join("core.db.part").exists());
        // Mtime carries the server's Last-Modified (2020-01-01T00:00:00Z).
        let mtime = fs::metadata(&dest).unwrap().modified().unwrap();
        // A unix timestamp is naturally seconds — "437732h" would obscure it.
        #[allow(clippy::duration_suboptimal_units)]
        let expect = SystemTime::UNIX_EPOCH + Duration::from_secs(1_577_836_800);
        assert_eq!(mtime, expect);
    }

    #[test]
    fn unchanged_when_the_server_304s_the_if_modified_since() {
        let dir = TempDir::new().unwrap();
        let dest = dir.path().join("core.db");
        fs::write(&dest, b"OLD").unwrap();
        let interrupt = AtomicBool::new(false);
        context::scope(|s| {
            let (port, headers) =
                one_shot_server(s, b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n");
            let url = format!("http://127.0.0.1:{port}/core.db");
            let spec = FetchSpec {
                url: &url,
                dest_dir: dir.path(),
                force: false,
                interrupt: &interrupt,
            };
            let out = spec.run(|_| {}).unwrap();
            assert_eq!(out, FetchOutcome::Unchanged);
            // The existing copy's mtime went out as If-Modified-Since.
            crate::assert_contains!(headers.recv().unwrap(), "If-Modified-Since");
        });
        assert_eq!(fs::read(&dest).unwrap(), b"OLD", "existing copy untouched");
        assert!(!dir.path().join("core.db.part").exists());
    }

    #[test]
    fn force_skips_the_if_modified_since_probe() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("core.db"), b"OLD").unwrap();
        let interrupt = AtomicBool::new(false);
        context::scope(|s| {
            let (port, headers) = one_shot_server(
                s,
                b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nNEW",
            );
            let url = format!("http://127.0.0.1:{port}/core.db");
            let spec = FetchSpec {
                url: &url,
                dest_dir: dir.path(),
                force: true,
                interrupt: &interrupt,
            };
            let out = spec.run(|_| {}).unwrap();
            assert_eq!(out, FetchOutcome::Downloaded);
            assert!(!headers.recv().unwrap().contains("If-Modified-Since"));
        });
        assert_eq!(fs::read(dir.path().join("core.db")).unwrap(), b"NEW");
    }

    #[test]
    fn http_error_fails_and_leaves_no_debris() {
        let dir = TempDir::new().unwrap();
        let interrupt = AtomicBool::new(false);
        context::scope(|s| {
            let (port, _headers) = one_shot_server(
                s,
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
            let url = format!("http://127.0.0.1:{port}/extra.db");
            let spec = FetchSpec {
                url: &url,
                dest_dir: dir.path(),
                force: false,
                interrupt: &interrupt,
            };
            let err = spec.run(|_| {}).unwrap_err();
            crate::assert_contains!(err.to_string(), "404");
        });
        assert!(!dir.path().join("extra.db").exists());
        assert!(!dir.path().join("extra.db.part").exists());
    }

    /// THE test for the module's reason to exist: a transfer parked on a
    /// silent socket aborts within curl's progress beat once the interrupt
    /// flag flips — it does not wait out the peer (or even the 10 s
    /// low-speed reaper).
    #[test]
    fn interrupt_aborts_a_stalled_transfer_promptly() {
        let dir = TempDir::new().unwrap();
        let interrupt = AtomicBool::new(false);
        // Dropped at scope end to release the stalled server thread.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        context::scope(|s| {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            s.spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                // Headers plus a taste of body, then silence: the client is
                // now parked mid-transfer, exactly like a hung mirror.
                (&stream)
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nPARTIAL")
                    .unwrap();
                (&stream).flush().unwrap();
                // Hold the socket open until the test finishes.
                release_rx.recv().ok();
            });

            let url = format!("http://127.0.0.1:{port}/core.db");
            let spec = FetchSpec {
                url: &url,
                dest_dir: dir.path(),
                force: false,
                interrupt: &interrupt,
            };
            let started = Instant::now();
            // The "user" hits Ctrl+C once the first bytes have arrived.
            let err = spec
                .run(|p| {
                    if p.downloaded.bytes() > 0 {
                        interrupt.store(true, Ordering::SeqCst);
                    }
                })
                .unwrap_err();
            let waited = started.elapsed();
            assert!(matches!(err, Error::Interrupted), "got {err:?}");
            assert!(
                waited < Duration::from_secs(8),
                "abort took {waited:?} — the interrupt should beat every timeout",
            );
            drop(release_tx);
        });
        assert!(!dir.path().join("core.db").exists());
        assert!(!dir.path().join("core.db.part").exists());
    }
}
