//! Rootless refresh of the official-repo sync databases — aurox's native
//! equivalent of `checkupdates(1)`, without the `fakeroot` dance.
//!
//! `pacman -Sy` needs root because it writes the downloaded DBs into
//! `DBPath/sync/` and the pacman *frontend* enforces `EUID == 0`. libalpm
//! itself enforces no such thing: `alpm_db_update` just writes wherever the
//! handle's dbpath points. So we open an [`Alpm`] handle aimed at a *private*,
//! user-writable dbpath (its `local` symlinked to the system one), register the
//! configured repos, and call [`update`] — a normal-user download into aurox's
//! state dir. No root, no `fakeroot`, no subprocess.
//!
//! The download itself is **not** libalpm's: its internal curl backend cannot
//! be aborted from outside (nothing external reaches its interrupt static —
//! pacman itself just `_Exit`s on Ctrl+C), so a Ctrl+C would sit out the whole
//! transfer. We register [`crate::pacman::dload`] as the handle's *fetch
//! callback* instead — libalpm then delegates every file (each repo DB and its
//! optional `.sig`) to our curl code, which draws one indicatif byte-row per
//! repo DB into the caller's shared [`MultiProgress`] and aborts within a beat
//! when the [`cancel_on_sigint`] guard wrapping the whole refresh flips its
//! flag. One fetch-callback call downloads one file, sequentially — with 3-5
//! small repo DBs per refresh, parallelism would buy nothing.
//!
//! The downloaded DBs persist between runs (incremental `If-Modified-Since`
//! fetches), and [`synced_db_path`] hands them to the upgrade-check readers
//! ([`crate::pacman::invoke::query_repo_upgrades`],
//! [`crate::build::collect_upgrade_plan`]).
//!
//! Two locks are in play during a refresh. libalpm's own `db.lck` (created
//! `O_EXCL` inside `update` and unlinked on return) serializes DB writers but
//! is a pure *existence* lock: it can't distinguish a live holder from the
//! orphan a killed refresh leaves behind, and libalpm never cleans that orphan
//! up — every later refresh just fails with "unable to lock database". aurox
//! therefore holds an advisory `flock` on the dbpath directory itself
//! ([`RefreshLock`]) across the whole critical section. The kernel releases a
//! flock when its holder dies, so *holding it* proves no concurrent refresh is
//! alive — which is what makes clearing a leftover `db.lck` sound, and what
//! makes waiting on a live one finite.
//!
//! [`update`]: alpm::AlpmList::update

use crate::context;
use crate::error::{Error, Result};
use crate::interrupt::cancel_on_sigint;
use crate::pacman::alpm_db;
use crate::pacman::dload::{FetchOutcome, FetchSpec};
use crate::paths;
use crate::ui;
use alpm::FetchResult;
use indicatif::MultiProgress;
use signal_hook::consts::SIGINT;
use signal_hook::iterator::Signals;
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;
use tracing::{debug, instrument, warn};

/// Outcome of a [`refresh_sync_db`] run, reported once the shared progress
/// display is torn down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncOutcome {
    /// At least one repo DB advanced to a newer copy.
    Refreshed,
    /// Every repo DB was already current (`If-Modified-Since` 304s).
    AlreadyCurrent,
}

/// Refresh the official-repo sync DBs into aurox's private dbpath, rootless.
///
/// Runs under a [`cancel_on_sigint`] guard: a Ctrl+C anywhere in the critical
/// section — the lock wait, or mid-download via [`DbFetcher`]'s abort — bails
/// out as [`Error::Interrupted`] instead of killing aurox or sitting out the
/// transfer. The guard's `Signals` coexists with the AUR half's own guard when
/// both run in parallel; one Ctrl+C reaches both.
///
/// Errors (unreadable `pacman.conf`, a download/verify failure, …) are returned
/// for the caller to downgrade to a warning — a repo-sync failure must never
/// fail the AUR refresh it runs beside.
#[instrument(skip(mp))]
pub fn refresh_sync_db(mp: &MultiProgress) -> Result<SyncOutcome> {
    cancel_on_sigint(|interrupt| refresh_guarded(mp, interrupt))
}

/// The critical section proper, with the guard's `interrupt` flag in hand.
///
/// Opens a mutable alpm handle at [`paths::sync_db_path`], registers
/// [`DbFetcher`] as the handle's fetch callback (our interruptible downloader —
/// see [`crate::pacman::dload`]), and runs [`alpm::AlpmList::update`] over
/// every registered sync DB. The handle is built and used entirely on the
/// calling thread (alpm is `!Sync`); only the shared [`MultiProgress`] crosses
/// threads, and that is safe.
fn refresh_guarded(mp: &MultiProgress, interrupt: &Arc<AtomicBool>) -> Result<SyncOutcome> {
    let db = paths::sync_db_path();
    prepare_db_dir(&db)?;
    // Hold the advisory refresh lock for the whole critical section: it waits
    // out a live concurrent refresh (Ctrl+C skips), and holding it is what
    // proves a leftover `db.lck` below is a dead process's orphan rather than
    // a live lock we'd be clobbering.
    let lock = RefreshLock::acquire(&db, mp)?;

    let mut alpm = alpm_db::open_at_for_refresh(&db)?;
    // A refresh killed mid-download strands libalpm's `db.lck`; clear that
    // orphan before updating or every subsequent sync fails with
    // `ALPM_ERR_HANDLE_LOCK` ("unable to lock database"). The path comes from
    // the handle, not a reconstructed guess (owned because `set_fetch_cb`
    // below needs the handle mutably).
    let alpm_lockfile = PathBuf::from(alpm.lockfile());
    lock.clear_stale_lock(&alpm_lockfile)?;
    // Delegate every download to aurox's interruptible fetcher. `DbFetcher` is
    // moved into the handle as the callback's user data and lives until the
    // handle drops at the end of this function.
    alpm.set_fetch_cb(
        DbFetcher {
            multi: mp.clone(),
            interrupt: Arc::clone(interrupt),
        },
        DbFetcher::on_fetch,
    );

    debug!(dbpath = %db.display(), "updating sync dbs (rootless)");
    // `update` wraps `alpm_db_update`, which returns 1 when *all* DBs were
    // already current and 0 when at least one was refreshed — so the bool is
    // "everything up to date", not "something changed".
    let all_current = alpm.syncdbs_mut().update(false).map_err(|e| {
        if lock.is_degraded() && matches!(e, alpm::Error::HandleLock) {
            // Without flock we skipped the stale-lock cleanup, so a stranded
            // `db.lck` needs the user's hand — name the file to remove.
            Error::other(format!(
                "sync db update: {e}; advisory locking is unavailable on this \
                 filesystem so a stale lock can't be cleared automatically — \
                 if no other aurox is refreshing, remove {} manually",
                alpm_lockfile.display(),
            ))
        } else {
            Error::other(format!("sync db update: {e}"))
        }
    })?;

    Ok(if all_current {
        SyncOutcome::AlreadyCurrent
    } else {
        SyncOutcome::Refreshed
    })
}

/// How often the contended wait in [`RefreshLock::wait_for_lock`] re-probes
/// the flock. flock has no "released" notification, so the wait must re-check;
/// between probes it blocks on the cancel channel, so Ctrl+C stays instant and
/// this only bounds how soon after the peer finishes we take over.
const LOCK_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Advisory exclusive lock over the private dbpath for the duration of a
/// refresh, anchored as a kernel `flock` on the dbpath *directory* itself.
///
/// The directory (rather than a sidecar lock file) is deliberate: a flock
/// follows the inode, so a sidecar that gets deleted and recreated would let
/// two processes each "hold" the lock on different inodes. The directory's
/// inode is stable and can't be unlinked while non-empty. Released on drop
/// (fd close) — and, crucially, by the kernel if the holder dies, which is
/// the liveness signal libalpm's own existence-based `db.lck` can't provide.
///
/// `flock` is `None` in degraded mode (filesystem without flock support):
/// the refresh still runs — libalpm's own `O_EXCL` on `db.lck` keeps
/// concurrent writers from corrupting each other — but with no way to prove
/// liveness, [`Self::clear_stale_lock`] refuses to touch `db.lck`.
#[derive(Debug)]
struct RefreshLock {
    flock: Option<File>,
}

impl RefreshLock {
    /// Take the refresh lock on `db`, waiting out a live concurrent refresh.
    ///
    /// Fast path: an uncontended non-blocking flock returns at once. If a
    /// peer holds it, wait for the release with Ctrl+C wired up as
    /// cancellation ([`Self::wait_interruptible`]). If the filesystem doesn't
    /// support flock at all, degrade with a warning instead of failing — see
    /// the type docs.
    fn acquire(db: &Path, mp: &MultiProgress) -> Result<Self> {
        let dir = File::open(db)?;
        match dir.try_lock() {
            Ok(()) => Ok(Self { flock: Some(dir) }),
            Err(TryLockError::WouldBlock) => Self::wait_interruptible(dir, mp),
            Err(TryLockError::Error(e)) => {
                warn!(
                    error = %e,
                    dir = %db.display(),
                    "advisory locking unavailable; concurrent-refresh \
                     serialization and stale-lock cleanup disabled",
                );
                Ok(Self { flock: None })
            }
        }
    }

    /// Wait for a live peer to release the lock, with Ctrl+C cancelling the
    /// wait as [`Error::Interrupted`].
    ///
    /// Same `Signals` pattern as `build::makepkg`: the handler suppresses the
    /// default die-on-SIGINT for the wait's duration, a watcher thread blocks
    /// on the signal pipe (no polling) and pings the cancel channel, and the
    /// RAII drop restores the previous disposition. The indirection is
    /// required — `signal_hook` registers with `SA_RESTART`, so a plain
    /// blocking `flock(2)` would be transparently restarted after Ctrl+C
    /// instead of unwinding.
    fn wait_interruptible(dir: File, mp: &MultiProgress) -> Result<Self> {
        // `suspend` so the notice isn't torn by the parallel AUR-fetch bars.
        mp.suspend(|| {
            ui::info(
                "another aurox refresh is running; waiting for it to finish \
                 (Ctrl+C skips the repo sync)",
            );
        });
        let mut signals = Signals::new([SIGINT])?;
        let handle = signals.handle();
        let (tx, rx) = mpsc::channel();
        context::scope(|s| {
            // Watcher: blocks on the signal pipe and pings the cancel channel
            // on each Ctrl+C; `handle.close()` ends it once the wait is over.
            s.spawn(move || {
                for _ in &mut signals {
                    // A send can only fail once the wait has already returned
                    // and dropped the receiver — nothing left to cancel.
                    tx.send(()).ok();
                }
            });
            let waited = Self::wait_for_lock(dir, &rx);
            handle.close();
            waited
        })
    }

    /// The wait loop proper: re-probe the flock, blocking on the cancel
    /// channel between probes. A ping on `cancel` aborts the wait as
    /// [`Error::Interrupted`]; the sender must stay alive while the wait runs
    /// (in production [`Self::wait_interruptible`]'s watcher holds it until
    /// the wait returns).
    fn wait_for_lock(dir: File, cancel: &Receiver<()>) -> Result<Self> {
        loop {
            // Probe first: the peer may have released between the caller's
            // failed fast path and now (or before the next re-probe).
            match dir.try_lock() {
                Ok(()) => return Ok(Self { flock: Some(dir) }),
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(e)) => return Err(e.into()),
            }
            match cancel.recv_timeout(LOCK_POLL_INTERVAL) {
                Ok(()) => return Err(Error::Interrupted),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    // No cancel source remains, so nothing can interrupt the
                    // wait anymore — finish with a plain blocking flock
                    // (kernel wakeup, no polling). Unreachable in production
                    // (the watcher outlives the wait); kept for correctness.
                    dir.lock()?;
                    return Ok(Self { flock: Some(dir) });
                }
            }
        }
    }

    /// Whether flock was unavailable and the refresh is running unserialized.
    const fn is_degraded(&self) -> bool {
        self.flock.is_none()
    }

    /// Remove a stale libalpm lock left in the private dbpath by an
    /// interrupted refresh, so the next update can acquire it.
    ///
    /// libalpm takes an exclusive `db.lck` (create-with-`O_EXCL`) for the
    /// duration of `alpm_db_update` and unlinks it when the call returns. A
    /// process killed mid-download never reaches that cleanup, stranding the
    /// lock — and every later refresh then fails with `ALPM_ERR_HANDLE_LOCK`
    /// ("unable to lock database") while `pacman -Sy` sails on, since that
    /// locks `/var/lib/pacman/db.lck`, a *different* file.
    ///
    /// Removal is sound *because* `self` holds the advisory refresh lock: no
    /// other aurox is inside its critical section, so a `db.lck` present now
    /// can only be a dead process's orphan. In degraded mode that proof is
    /// unavailable and the file is left untouched. Absent lock ⇒ no-op.
    fn clear_stale_lock(&self, lockfile: &Path) -> Result<()> {
        if self.is_degraded() {
            debug!(
                lockfile = %lockfile.display(),
                "no advisory lock held; leaving the alpm lockfile untouched",
            );
            return Ok(());
        }
        match std::fs::remove_file(lockfile) {
            Ok(()) => {
                warn!(
                    lockfile = %lockfile.display(),
                    "cleared a stale sync-db lock left by an interrupted refresh",
                );
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// aurox's private dbpath, but only once it's actually usable — at least one
/// downloaded `*.db` under `sync/` and a `local` symlink that still resolves.
///
/// Returning `None` until both hold is load-bearing: an empty or half-built
/// store would report *every* installed package as foreign (no sync repo
/// declares it), so the upgrade-check readers fall back to the system dbpath
/// until the first successful [`refresh_sync_db`].
pub fn synced_db_path() -> Option<PathBuf> {
    let db = paths::sync_db_path();
    // `exists()` follows the symlink, so a dangling `local` reads as absent.
    if !db.join("local").exists() {
        return None;
    }
    let has_db = match std::fs::read_dir(db.join("sync")) {
        Ok(entries) => entries
            .flatten()
            .any(|e| e.path().extension().is_some_and(|ext| ext == "db")),
        Err(_) => false,
    };
    has_db.then_some(db)
}

/// Create the private dbpath and point its `local` at the system localdb.
///
/// pacman/libalpm create `sync/` themselves on first update; we only need the
/// dbpath root plus the `local` symlink so alpm reads the real installed set.
fn prepare_db_dir(db: &Path) -> Result<()> {
    std::fs::create_dir_all(db)?;
    let system_local = alpm_db::system_db_path()?.join("local");
    ensure_symlink(&system_local, &db.join("local"))
}

/// Idempotently make `link` a symlink to `target`. Re-points a link aimed
/// elsewhere (e.g. the system dbpath moved) and clears a non-symlink sitting in
/// the way; a no-op when it already points where we want.
fn ensure_symlink(target: &Path, link: &Path) -> Result<()> {
    match std::fs::read_link(link) {
        Ok(current) if current == target => return Ok(()),
        Ok(_) => std::fs::remove_file(link)?,
        // Not a symlink but something is there — clear it so we can relink.
        Err(_) if link.exists() => std::fs::remove_file(link)?,
        Err(_) => {}
    }
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

/// libalpm's fetch callback: downloads one file per call through
/// [`crate::pacman::dload`], rendering repo DBs as one indicatif byte-row each
/// in the shared [`MultiProgress`] so they line up with the AUR fetch rows.
/// Moved into the alpm handle as `set_fetch_cb` user data.
struct DbFetcher {
    multi: MultiProgress,
    /// The [`cancel_on_sigint`] guard's flag; a Ctrl+C aborts the transfer
    /// in flight from curl's progress meter.
    interrupt: Arc<AtomicBool>,
}

impl DbFetcher {
    /// `set_fetch_cb` callback: fetch `url` into the directory `dest_dir`.
    ///
    /// The return value speaks libalpm's protocol: [`FetchResult::Ok`] = a new
    /// copy landed, [`FetchResult::FileExists`] = unchanged since the copy on
    /// disk (its mtime, sent as `If-Modified-Since`, is how "unchanged" is
    /// known), [`FetchResult::Err`] = this URL failed — libalpm then tries the
    /// repo's next server, so a hard failure surfaces from `update()` only
    /// once every server declined. Only repo DBs get a progress row; their
    /// detached signatures (`*.db.sig`, requested per the repo's siglevel) are
    /// tiny and would just add noise.
    fn on_fetch(url: &str, dest_dir: &str, force: bool, this: &mut Self) -> FetchResult {
        let spec = FetchSpec {
            url,
            dest_dir: Path::new(dest_dir),
            force,
            interrupt: &this.interrupt,
        };
        let name = url.rsplit('/').next().unwrap_or(url);
        let bar = Path::new(name)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("db"))
            .then(|| {
                let bar = this.multi.add(ui::bar_bytes_streaming(name));
                ui::tick(&bar);
                bar
            });
        let outcome = spec.run(|progress| {
            if let Some(bar) = &bar {
                if let Some(total) = progress.total {
                    ui::promote_byte_bar(bar, total.bytes());
                }
                bar.set_position(progress.downloaded.bytes());
            }
        });
        if let Some(bar) = bar {
            bar.finish_and_clear();
        }
        match outcome {
            Ok(FetchOutcome::Downloaded) => FetchResult::Ok,
            Ok(FetchOutcome::Unchanged) => FetchResult::FileExists,
            Err(e) => {
                // Best-effort per-URL diagnostics only: an optional `.sig`
                // 404ing here is routine, and a real failure is reported by
                // the caller once `update()` gives up.
                debug!(url, error = %e, "repo-db fetch failed");
                FetchResult::Err
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RefreshLock, ensure_symlink, synced_db_path};
    use crate::context;
    use crate::error::Error;
    use crate::paths;
    use crate::testing::ScopedStateRoot;
    use std::fs::{self, File, TryLockError};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;
    use tempfile::TempDir;

    #[test]
    fn ensure_symlink_creates_retargets_and_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("target-a");
        let b = dir.path().join("target-b");
        fs::create_dir(&a).unwrap();
        fs::create_dir(&b).unwrap();
        let link = dir.path().join("local");

        // Created from nothing.
        ensure_symlink(&a, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), a);
        // Idempotent — already correct, left as-is.
        ensure_symlink(&a, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), a);
        // Re-pointed when the target changes.
        ensure_symlink(&b, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), b);
    }

    #[test]
    fn ensure_symlink_replaces_a_plain_file() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        let link = dir.path().join("local");
        // A regular file squatting where the symlink should go.
        fs::write(&link, b"stale").unwrap();

        ensure_symlink(&target, &link).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), target);
    }

    /// THE regression test for the unconditional-clear bug: while another
    /// process holds the advisory refresh lock (i.e. a *live* refresh is in
    /// flight), a second refresh must neither proceed nor touch the live
    /// holder's `db.lck` — and once the holder releases, it proceeds and
    /// clears the (now provably orphaned) lockfile.
    ///
    /// The "must not clobber" half is not timing-dependent: the waiter cannot
    /// pass `wait_for_lock` while the holder's kernel flock is held, so the
    /// `db.lck` assertion rests on flock's exclusion guarantee, not on sleeps.
    #[test]
    fn waits_out_live_holder_without_touching_db_lck() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path();
        let lck = db.join("db.lck");
        // The live peer's alpm lock, mid-update().
        fs::write(&lck, b"").unwrap();

        // "Another aurox": a distinct fd holding the advisory lock. flock
        // conflicts are per open-file-description, so a second open of the
        // same dir contends exactly like a second process would.
        let holder = File::open(db).unwrap();
        holder.try_lock().unwrap();

        let (_cancel_tx, cancel_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel();
        context::scope(|s| {
            s.spawn(move || {
                let guard =
                    RefreshLock::wait_for_lock(File::open(db).unwrap(), &cancel_rx).unwrap();
                guard.clear_stale_lock(&db.join("db.lck")).unwrap();
                done_tx.send(()).unwrap();
            });

            // While the holder lives the waiter must not finish — and above
            // all must not delete the live db.lck.
            assert_eq!(
                done_rx.recv_timeout(Duration::from_millis(200)),
                Err(RecvTimeoutError::Timeout),
                "second refresh barged past a live advisory lock",
            );
            assert!(lck.exists(), "live db.lck was clobbered");

            // Peer finishes: release the flock; the waiter takes over and may
            // now clear the leftover lockfile.
            holder.unlock().unwrap();
            done_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("waiter never acquired the lock after release");
            assert!(!lck.exists(), "orphan db.lck not cleared after acquisition");
        });
    }

    /// Ctrl+C — surfaced as a ping on the cancel channel — aborts the wait as
    /// `Error::Interrupted` and leaves the live holder's `db.lck` alone. The
    /// ping is pre-queued so the wait sees it on its first block: no timing.
    #[test]
    fn cancel_interrupts_the_wait() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path();
        let lck = db.join("db.lck");
        fs::write(&lck, b"").unwrap();
        let holder = File::open(db).unwrap();
        holder.try_lock().unwrap();

        let (cancel_tx, cancel_rx) = mpsc::channel();
        cancel_tx.send(()).unwrap();
        let err = RefreshLock::wait_for_lock(File::open(db).unwrap(), &cancel_rx).unwrap_err();
        assert!(matches!(err, Error::Interrupted), "got {err:?}");
        assert!(lck.exists());
    }

    /// The lock is a real kernel flock on the dbpath directory: while held, a
    /// probe fd would-block; after drop, the probe acquires. Also proves
    /// directory-fd flocks work on this platform — the scheme's load-bearing
    /// assumption.
    #[test]
    fn lock_is_exclusive_until_dropped() {
        let tmp = TempDir::new().unwrap();
        let db = tmp.path();
        let (_tx, rx) = mpsc::channel::<()>();
        let guard = RefreshLock::wait_for_lock(File::open(db).unwrap(), &rx).unwrap();

        let probe = File::open(db).unwrap();
        assert!(matches!(probe.try_lock(), Err(TryLockError::WouldBlock)));
        drop(guard);
        probe.try_lock().unwrap();
    }

    /// Holding the lock: an orphan `db.lck` is removed, an absent one is a
    /// no-op (the common clean-run case).
    #[test]
    fn clear_stale_lock_unlinks_orphan_and_ignores_absent() {
        let tmp = TempDir::new().unwrap();
        let lck = tmp.path().join("db.lck");
        let (_tx, rx) = mpsc::channel::<()>();
        let guard = RefreshLock::wait_for_lock(File::open(tmp.path()).unwrap(), &rx).unwrap();

        // No lock present → no-op, still Ok.
        guard.clear_stale_lock(&lck).unwrap();

        // An orphan from an interrupted refresh (libalpm creates it 0000) →
        // removed, so the next update can reacquire it.
        fs::write(&lck, b"").unwrap();
        guard.clear_stale_lock(&lck).unwrap();
        assert!(!lck.exists());
    }

    /// Degraded mode (flock unsupported): liveness can't be proven, so the
    /// cleanup must leave `db.lck` untouched no matter what.
    #[test]
    fn degraded_lock_never_touches_db_lck() {
        let tmp = TempDir::new().unwrap();
        let lck = tmp.path().join("db.lck");
        fs::write(&lck, b"").unwrap();

        let guard = RefreshLock { flock: None };
        assert!(guard.is_degraded());
        guard.clear_stale_lock(&lck).unwrap();
        assert!(lck.exists(), "degraded mode removed a possibly-live db.lck");
    }

    #[test]
    fn synced_db_path_requires_local_link_and_a_sync_db() {
        let dir = TempDir::new().unwrap();
        let _root = ScopedStateRoot::new(dir.path().to_path_buf());
        let db = paths::sync_db_path();
        let sync = db.join("sync");
        fs::create_dir_all(&sync).unwrap();

        // A `*.db` but no `local` link → fall back to the system db.
        fs::write(sync.join("core.db"), b"x").unwrap();
        assert!(synced_db_path().is_none());

        // `local` present but no `*.db` → still incomplete.
        fs::remove_file(sync.join("core.db")).unwrap();
        let real_local = dir.path().join("real-local");
        fs::create_dir(&real_local).unwrap();
        std::os::unix::fs::symlink(&real_local, db.join("local")).unwrap();
        assert!(synced_db_path().is_none());

        // Both present → the private dbpath is usable.
        fs::write(sync.join("extra.db"), b"x").unwrap();
        assert_eq!(synced_db_path(), Some(db));
    }

    #[test]
    fn synced_db_path_rejects_a_dangling_local_link() {
        let dir = TempDir::new().unwrap();
        let _root = ScopedStateRoot::new(dir.path().to_path_buf());
        let db = paths::sync_db_path();
        let sync = db.join("sync");
        fs::create_dir_all(&sync).unwrap();
        fs::write(sync.join("core.db"), b"x").unwrap();
        // Points at a path that doesn't exist — `exists()` follows it and reads
        // false, so this dbpath must not be handed out.
        std::os::unix::fs::symlink(dir.path().join("gone"), db.join("local")).unwrap();
        assert!(synced_db_path().is_none());
    }
}
