//! Adapter wiring gix's progress trait family onto our indicatif bars.
//!
//! One shared summary line carries gix's most-recent `message()`. Each
//! gix child that actually emits step progress (`init` or `set`/`inc_by`)
//! owns its **own** leaf bar — created lazily on first such call, removed
//! from the `MultiProgress` when the child drops. Result: phases that emit
//! nothing don't stack rows, and concurrent children (e.g. `remote` +
//! `read pack`) coexist on screen the way `git clone` shows them.

use super::dim;
use super::progress::{
    bar_bytes_streaming, bar_count, bar_sideband, idle_byte_bar, promote_byte_bar,
    promote_count_bar, resume_byte_bar, tick,
};
use crate::context;

use gix::progress::prodash::progress::Step;
use gix::progress::{Count as GixCount, Id, MessageLevel, Unit};
use gix::{NestedProgress, Progress as GixProgressTrait};
use indicatif::{MultiProgress, ProgressBar};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Adapter implementing [`gix::Progress`] / [`gix::NestedProgress`] on top of
/// our indicatif bars.
pub struct GixProgress {
    shared: Arc<Shared>,
    /// Sub-phase name this clone owns (used as fallback when gix never
    /// calls `set_name` after `init`).
    own_name: String,
    /// Whether `init` was called with `progress::bytes()`. Drives whether
    /// the leaf formats `{bytes}`/`{binary_bytes_per_sec}` or raw counts.
    own_unit_is_bytes: bool,
    /// Max recorded by `init`/`set_max`; applied to the leaf the first
    /// time it actually gets created.
    own_max: Option<u64>,
    /// This node's own leaf bar (lazy). `None` until the node actually
    /// reports step progress (`set` or `inc_by`); cleared from the
    /// `MultiProgress` on `Drop`. Nodes that only get `set_name`'d (root,
    /// intermediate ancestors) never spawn a leaf.
    leaf: Mutex<Option<ProgressBar>>,
}

/// State shared by every node in one progress tree.
struct Shared {
    multi: MultiProgress,
    summary: ProgressBar,
    /// Always-on wire-throughput row, fed by [`NetMeter`]'s pump thread.
    net: NetMeter,
}

/// A persistent `network` byte bar plus the background thread that pumps it.
///
/// gix reports progress only for phases it drives itself (the pack receive);
/// the v2 `ls-refs` advertisement — which on a 155k-ref mirror is ~11 MiB and
/// can take minutes — is read inside gix with no progress callbacks at all. The
/// bytes do flow through the curl backend, which adds each received chunk to
/// `counter` (via `http::Options::download_progress`). Since the main thread is
/// blocked in gix during that read and can't tick a bar, a dedicated thread
/// samples `counter` into `bar` so the row shows live bytes + speed regardless
/// of which phase is running.
struct NetMeter {
    /// Cumulative response-body bytes; shared with the curl backend.
    counter: Arc<AtomicU64>,
    /// The `network` row in the [`Shared::multi`].
    bar: ProgressBar,
    /// Set to stop the pump thread.
    stop: Arc<AtomicBool>,
    /// Pump handle; `take`n by the first [`NetMeter::stop_and_clear`].
    handle: Mutex<Option<JoinHandle<()>>>,
}

/// Trailing window over which wire activity is judged: the `network` row is
/// parked in its idle style once fewer than [`NET_ACTIVE_BYTES`] arrived
/// within it. Long enough that an ordinary TCP stall mid-stream doesn't flap
/// the style; short compared to the minutes-long silent stretches it exists
/// for (server-side packing, post-pack local work).
const NET_IDLE_AFTER: Duration = Duration::from_secs(2);

/// Bytes that must arrive within one [`NET_IDLE_AFTER`] window for the wire
/// to count as active. While the server packs, sideband keep-alives and
/// `remote: Counting objects…` progress lines trickle in at tens of bytes per
/// second — judged by "any new byte" they flapped the row between idle and a
/// stale decaying rate every couple of seconds. Real pack streaming clears
/// this ~4 KiB/s bar by orders of magnitude; the trickle never comes close.
const NET_ACTIVE_BYTES: u64 = 8 * 1024;

/// Whether bytes are currently flowing on the wire, as judged by [`IdleTracker`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WireState {
    Active,
    Idle,
}

/// Wire-activity detector for the [`NetMeter`] pump.
///
/// indicatif's rate field keeps re-weighting its estimate by elapsed time, so
/// when bytes stop flowing the shown speed doesn't drop to zero — it *decays*
/// there over ~15 s, which reads as the download regressing. The tracker
/// watches the raw counter instead — but not for *any* byte: the counter also
/// sees the sideband keep-alives and `remote: …` progress lines the server
/// trickles while packing, and judged byte-for-byte those flap the row at
/// their own cadence. The wire counts as [`WireState::Active`] only while at
/// least [`NET_ACTIVE_BYTES`] arrived within the trailing [`NET_IDLE_AFTER`]
/// window; each crossing of that line is reported exactly once.
struct IdleTracker {
    /// `(sample instant, cumulative byte count)`, newest at the back. The
    /// front entry is kept just outside the window as the baseline, so the
    /// byte delta always spans at least [`NET_IDLE_AFTER`].
    samples: VecDeque<(Instant, u64)>,
    idle: bool,
}

impl IdleTracker {
    fn new(now: Instant) -> Self {
        Self {
            samples: VecDeque::from([(now, 0)]),
            idle: false,
        }
    }

    /// Digest one pump sample; returns the new state when it flipped.
    fn observe(&mut self, pos: u64, now: Instant) -> Option<WireState> {
        self.samples.push_back((now, pos));
        while self.samples.len() > 1 && now.duration_since(self.samples[1].0) >= NET_IDLE_AFTER {
            self.samples.pop_front();
        }
        let baseline = self.samples[0].1;
        let active = pos.saturating_sub(baseline) >= NET_ACTIVE_BYTES;
        match (self.idle, active) {
            (false, false) => {
                self.idle = true;
                Some(WireState::Idle)
            }
            (true, true) => {
                self.idle = false;
                Some(WireState::Active)
            }
            _ => None,
        }
    }
}

impl NetMeter {
    /// Add the `network` row to `multi` and spawn the pump that mirrors
    /// `counter` into it every ~120 ms, parking the row in its idle style
    /// whenever the wire goes quiet (see [`IdleTracker`]).
    fn spawn(multi: &MultiProgress) -> Self {
        let counter = Arc::new(AtomicU64::new(0));
        let bar = multi.add(bar_bytes_streaming("network"));
        tick(&bar);
        let stop = Arc::new(AtomicBool::new(false));
        let handle = context::spawn({
            let counter = Arc::clone(&counter);
            let bar = bar.clone();
            let stop = Arc::clone(&stop);
            move || {
                let mut wire = IdleTracker::new(Instant::now());
                while !stop.load(Ordering::Relaxed) {
                    let pos = counter.load(Ordering::Relaxed);
                    match wire.observe(pos, Instant::now()) {
                        Some(WireState::Idle) => idle_byte_bar(&bar),
                        Some(WireState::Active) => resume_byte_bar(&bar),
                        None => {}
                    }
                    bar.set_position(pos);
                    std::thread::sleep(Duration::from_millis(120));
                }
            }
        });
        Self {
            counter,
            bar,
            stop,
            handle: Mutex::new(Some(handle)),
        }
    }

    /// Stop the pump, join it, and clear the row. Idempotent: the handle is
    /// taken once, so a later call (e.g. from `Drop`) is a no-op.
    fn stop_and_clear(&self) {
        self.stop.store(true, Ordering::Relaxed);
        // Bind the `take` out of the guard first so the lock isn't held across
        // the `join` (matches the leaf-clearing idiom in `finish`/`Drop`).
        let handle = self.handle.lock().unwrap().take();
        if let Some(h) = handle {
            h.join().ok();
        }
        self.bar.finish_and_clear();
    }
}

impl Drop for NetMeter {
    fn drop(&mut self) {
        self.stop_and_clear();
    }
}

/// Detect a byte-unit by asking the unit to format its own label.
///
/// In prodash 31, `Bytes::display_unit` writes nothing (the suffix is baked
/// into the value via `bytesize::ByteSize`), while every count-style unit
/// (`Human`, `Range`) writes its name (`"objects"`, `"steps"`, ...). So an
/// empty `display_unit` output uniquely identifies bytes — no string
/// matching, no heuristic.
fn unit_is_bytes(unit: &Unit) -> bool {
    let mut s = String::new();
    unit.as_display_value().display_unit(&mut s, 0).ok();
    s.is_empty()
}

impl GixProgress {
    /// Create a fresh adapter with its own private `MultiProgress`. Stages just
    /// the summary line; leaves spawn lazily as gix children emit progress.
    pub fn new(label: &str) -> Self {
        Self::with_multi(label, MultiProgress::new())
    }

    /// Like [`new`](Self::new) but draws into a caller-supplied `MultiProgress`,
    /// so the fetch's rows share one display with other concurrent progress
    /// (e.g. the parallel official-repo db sync in `mirror::cmd_refresh`). Two
    /// separate `MultiProgress` instances would fight over the terminal.
    pub fn with_multi(label: &str, mp: MultiProgress) -> Self {
        let summary = mp.add(bar_sideband(label));
        summary.set_message("starting…");
        tick(&summary);
        let net = NetMeter::spawn(&mp);
        Self {
            shared: Arc::new(Shared {
                multi: mp,
                summary,
                net,
            }),
            own_name: String::new(),
            own_unit_is_bytes: false,
            own_max: None,
            leaf: Mutex::new(None),
        }
    }

    /// The cumulative download-byte counter the curl backend should write to.
    /// Hand this to `mirror::http_transport_options` so the `network` row
    /// reflects live wire throughput.
    pub fn net_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.shared.net.counter)
    }

    /// Clear all live bars. Intended for end-of-clone cleanup.
    pub fn finish(&self) {
        let taken = self.leaf.lock().unwrap().take();
        if let Some(pb) = taken {
            pb.finish_and_clear();
        }
        self.shared.net.stop_and_clear();
        self.shared.summary.finish_and_clear();
    }

    fn set_summary(&self, msg: String) {
        self.shared.summary.set_message(msg);
    }

    fn lock_leaf(&self) -> MutexGuard<'_, Option<ProgressBar>> {
        self.leaf.lock().unwrap()
    }

    /// Create or replace this node's own leaf bar with the configured style.
    fn restart_leaf(&self, name: &str) {
        let pb = if self.own_unit_is_bytes {
            self.shared.multi.add(bar_bytes_streaming(leaf_label(name)))
        } else {
            self.shared.multi.add(bar_count(0, leaf_label(name)))
        };
        tick(&pb);
        let mut g = self.lock_leaf();
        if let Some(old) = g.replace(pb) {
            old.finish_and_clear();
        }
    }

    /// Ensure a leaf exists with the current style; called lazily by `set`/`inc_by`.
    /// Applies any `own_max` that `init`/`set_max` recorded earlier. Returns
    /// without creating anything for muted phases (e.g. server-sideband echo).
    fn ensure_leaf(&self) {
        if self.lock_leaf().is_some() {
            return;
        }
        if leaf_is_muted(&self.own_name) {
            return;
        }
        let name = if self.own_name.is_empty() {
            "phase".to_owned()
        } else {
            self.own_name.clone()
        };
        self.restart_leaf(&name);
        if let Some(m) = self.own_max
            && let Some(pb) = self.lock_leaf().as_ref()
        {
            if self.own_unit_is_bytes {
                promote_byte_bar(pb, m);
            } else {
                promote_count_bar(pb, m);
            }
        }
    }

    fn update_leaf(&self, step: u64, max: Option<u64>) {
        self.ensure_leaf();
        let g = self.lock_leaf();
        if let Some(pb) = g.as_ref() {
            if let Some(m) = max {
                if self.own_unit_is_bytes {
                    promote_byte_bar(pb, m);
                } else {
                    promote_count_bar(pb, m);
                }
            }
            pb.set_position(step);
        }
    }
}

impl Drop for GixProgress {
    fn drop(&mut self) {
        let taken = self.leaf.lock().unwrap().take();
        if let Some(pb) = taken {
            pb.finish_and_clear();
        }
    }
}

/// Condense gix's long phase names into our fixed 14-wide prefix column.
fn leaf_label(name: &str) -> &str {
    match name.to_ascii_lowercase().as_str() {
        "receiving objects" => "objects",
        "indexing" | "resolving deltas" => "deltas",
        "decompressing" => "decompress",
        "read pack" => "pack",
        _ => name,
    }
}

/// Map known gix phase names to a one-line user-facing hint. The hint tells
/// the user what gix is *actually* doing and gives a rough ETA so the silent
/// phases don't look stuck. Returns `None` for unknown phases; in that case
/// the summary shows just the raw gix name.
///
/// ETAs are calibrated for `github.com/archlinux/aur` (~155 k refs, ~2 GiB
/// pack) on a residential connection; smaller repos finish faster.
fn phase_hint(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("handshake") {
        Some("TLS + HTTP smart-protocol setup")
    } else if lower == "authentication" {
        Some("authenticating with server")
    } else if lower == "list refs" {
        Some("downloading ref list (~20 s)")
    } else if lower.starts_with("negotiate") {
        Some("sending wants/haves to server")
    } else if lower == "receiving pack" {
        Some("server is packing objects, then streaming to us (~5–8 min)")
    } else if lower == "read pack" {
        Some("silent until server finishes packing (~3–5 min server-side, ~2–3 min stream)")
    } else if lower == "remote" {
        Some("server-side progress (counting / compressing objects)")
    } else if lower == "indexing" || lower == "resolving deltas" || lower == "resolving" {
        Some("local delta resolution (CPU-heavy, ~1–2 min)")
    } else if lower.starts_with("decompress") || lower == "decoding" {
        Some("decompressing pack entries")
    } else if lower == "sorting by id" {
        Some("sorting pack entries (brief)")
    } else if lower == "writing index file" {
        Some("writing pack index — finishing up")
    } else if lower == "create index file" {
        Some("building pack index")
    } else if lower.contains("fetch") {
        // After the last visible bar (Resolving), gix runs `update_refs` to
        // write every received ref to disk; that step emits no progress for
        // ~30 s – 2 min on a 155 k-ref mirror. So when we're back in the
        // outer "fetch" name with no active child bars, mention it.
        Some("finalizing — writing refs silently (~30 s – 2 min)")
    } else {
        None
    }
}

/// Phases whose progress is essentially noise we'd rather hide — the server's
/// sideband-translated "remote: Counting objects" / "remote: Compressing
/// objects" lines, which gix re-emits as a child whose name is the full server
/// string. The information is already visible in the summary row when the
/// message arrives; a dedicated bar with a 28-character prefix just breaks
/// alignment.
fn leaf_is_muted(name: &str) -> bool {
    name.starts_with("remote") || name.starts_with("remote:")
}

/// Build the summary text for a phase name, appending the hint (dimmed) when
/// one exists. The phase name stays at full brightness so the eye locks onto
/// it; the hint is supporting context.
fn summary_with_hint(name: &str) -> String {
    match phase_hint(name) {
        Some(hint) => format!("{name} {}", dim(format!("— {hint}"))),
        None => name.to_owned(),
    }
}

impl GixCount for GixProgress {
    fn set(&self, step: Step) {
        tracing::trace!(target: "gix_progress", phase = %self.own_name, step, "set");
        self.update_leaf(step as u64, None);
    }

    fn step(&self) -> Step {
        self.lock_leaf()
            .as_ref()
            .map_or(0, |pb| Step::try_from(pb.position()).unwrap_or(Step::MAX))
    }

    fn inc_by(&self, step: Step) {
        tracing::trace!(target: "gix_progress", phase = %self.own_name, step, "inc_by");
        self.ensure_leaf();
        if let Some(pb) = self.lock_leaf().as_ref() {
            pb.inc(step as u64);
        }
    }

    fn counter(&self) -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }
}

impl GixProgressTrait for GixProgress {
    fn init(&mut self, max: Option<Step>, unit: Option<Unit>) {
        self.own_unit_is_bytes = unit.as_ref().is_some_and(unit_is_bytes);
        self.own_max = max.map(|m| m as u64);
        // TRACE, not DEBUG: gix re-`init`s its internal progress nodes hundreds
        // of times per fetch as it walks pack indices / refs / etc. The real
        // state changes are `add_child` (new phase) and `message` (sideband).
        tracing::trace!(
            target: "gix_progress",
            phase = %self.own_name,
            ?max,
            is_bytes = self.own_unit_is_bytes,
            "init"
        );
        // Don't spawn a leaf yet. Many gix nodes call `init` once at startup
        // and then only emit `set_name` afterwards — those should never get
        // a row of their own. The leaf is created on the first `set`/`inc_by`.
        // If we already have a leaf and `init` is being called again to
        // declare a length (e.g. the sideband-translated "Counting objects"
        // line setting a max after earlier max=None messages), promote in
        // place so the bar style matches the new bound.
        if let (Some(m), Some(pb)) = (self.own_max, self.lock_leaf().as_ref()) {
            if self.own_unit_is_bytes {
                promote_byte_bar(pb, m);
            } else {
                promote_count_bar(pb, m);
            }
        }
    }

    fn unit(&self) -> Option<Unit> {
        None
    }

    fn max(&self) -> Option<Step> {
        self.lock_leaf()
            .as_ref()
            .and_then(|pb| pb.length().map(|x| Step::try_from(x).unwrap_or(Step::MAX)))
    }

    fn set_max(&mut self, max: Option<Step>) -> Option<Step> {
        self.own_max = max.map(|m| m as u64);
        if let Some(m) = max {
            // Only resize the bar if it already exists; don't spawn one here.
            if let Some(pb) = self.lock_leaf().as_ref() {
                if self.own_unit_is_bytes {
                    promote_byte_bar(pb, m as u64);
                } else {
                    promote_count_bar(pb, m as u64);
                }
            }
        }
        max
    }

    fn set_name(&mut self, name: String) {
        // Dedupe: gix re-emits the same name on every progress tick (e.g.
        // "remote: Counting objects" hundreds of times). Only the actual phase
        // *transitions* are interesting — those become DEBUG.
        if name != self.own_name {
            tracing::debug!(target: "gix_progress", old = %self.own_name, new = %name, "set_name");
        }
        self.set_summary(summary_with_hint(&name));
        self.own_name.clone_from(&name);
        if let Some(pb) = self.lock_leaf().as_ref() {
            pb.set_prefix(leaf_label(&name).to_owned());
        }
    }

    fn name(&self) -> Option<String> {
        Some(self.own_name.clone())
    }

    fn id(&self) -> Id {
        *b"GITA"
    }

    fn message(&self, _level: MessageLevel, message: String) {
        tracing::debug!(target: "gix_progress", phase = %self.own_name, %message, "message");
        // Synthesized marker: gix emits no `set_name`/`add_child` for the
        // ~15–20s post-pack ref-update phase, so the log goes silent right
        // when most users start wondering if it's hung. The only event gix
        // does fire beforehand is this "read pack" wrap-up message — promote
        // it into an explicit "next phase begins" line.
        let synth_post_pack_marker = self.own_name == "read pack" && message.starts_with("done");
        self.set_summary(message);
        if synth_post_pack_marker {
            tracing::debug!(
                target: "gix_progress",
                "post-pack phase begins: updating refs and writing pack manifest (silent in gix)"
            );
            // The wire is done for good once the pack is fully read — what
            // remains (ref updates, pack manifest) is local. Drop the
            // `network` row now rather than leaving it parked on `(idle)`
            // for the ~15 s – 2 min tail.
            self.shared.net.stop_and_clear();
        }
    }
}

impl NestedProgress for GixProgress {
    type SubProgress = Self;

    fn add_child(&mut self, name: impl Into<String>) -> Self::SubProgress {
        let name = name.into();
        tracing::debug!(target: "gix_progress", parent = %self.own_name, child = %name, "add_child");
        self.set_summary(summary_with_hint(&name));
        Self {
            shared: Arc::clone(&self.shared),
            own_name: name,
            own_unit_is_bytes: false,
            own_max: None,
            leaf: Mutex::new(None),
        }
    }

    fn add_child_with_id(&mut self, name: impl Into<String>, _id: Id) -> Self::SubProgress {
        self.add_child(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// Wait (bounded) for the pump to mirror `counter` into `bar`. Returns the
    /// observed position so callers can assert; bounded so a slow box can't
    /// hang the suite if the pump is broken.
    fn wait_for_position(bar: &ProgressBar, want: u64) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(2);
        while bar.position() != want && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        bar.position()
    }

    #[test]
    fn net_counter_feeds_the_visible_bar() {
        // The whole feature hinges on this seam: the counter handed to the curl
        // backend (`net_counter`) is the one the pump mirrors into the row the
        // user sees. Bytes written to it must surface on the bar.
        let progress = GixProgress::new("test");
        progress.net_counter().fetch_add(4096, Ordering::Relaxed);
        assert_eq!(wait_for_position(&progress.shared.net.bar, 4096), 4096);
        progress.finish();
    }

    #[test]
    fn stop_and_clear_joins_and_is_idempotent() {
        let meter = NetMeter::spawn(&MultiProgress::new());
        meter.stop_and_clear();
        assert!(
            meter.handle.lock().unwrap().is_none(),
            "pump handle should be taken+joined"
        );
        meter.stop_and_clear(); // second call must not panic / double-join
    }

    /// The rate field lies once the wire goes quiet (indicatif decays it to
    /// zero instead of dropping it), so the tracker must flip to `Idle` once
    /// the window drains and back to `Active` on a real byte burst — firing
    /// each transition exactly once.
    #[test]
    fn idle_tracker_flips_on_silence_and_back_on_bytes() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut wire = IdleTracker::new(t0);
        assert_eq!(
            wire.observe(NET_ACTIVE_BYTES, ms(120)),
            None,
            "streaming: already active"
        );
        assert_eq!(
            wire.observe(NET_ACTIVE_BYTES, ms(1_000)),
            None,
            "quiet, but the burst is still inside the window"
        );
        assert_eq!(
            wire.observe(NET_ACTIVE_BYTES, ms(2_200)),
            Some(WireState::Idle),
            "burst aged out of the window"
        );
        assert_eq!(
            wire.observe(NET_ACTIVE_BYTES, ms(60_000)),
            None,
            "idle fires only once"
        );
        assert_eq!(
            wire.observe(2 * NET_ACTIVE_BYTES, ms(60_120)),
            Some(WireState::Active),
            "real transfer resumed"
        );
        assert_eq!(
            wire.observe(3 * NET_ACTIVE_BYTES, ms(60_240)),
            None,
            "active fires only once"
        );
    }

    /// Before the first byte (DNS/TLS/handshake) the counter sits at zero;
    /// that's silence too, and the row should park right away rather than
    /// show a zero rate.
    #[test]
    fn idle_tracker_parks_before_first_byte() {
        let t0 = Instant::now();
        let mut wire = IdleTracker::new(t0);
        assert_eq!(
            wire.observe(0, t0 + Duration::from_millis(120)),
            Some(WireState::Idle)
        );
    }

    /// While the server packs, sideband keep-alives and `remote: …` progress
    /// lines trickle tens of bytes every second or two. Judged byte-for-byte
    /// that flapped the row idle↔active at the trickle cadence — each flap
    /// flashing a stale decaying rate — so the trickle must stay parked, and
    /// only the real pack stream may un-park the row.
    #[test]
    fn idle_tracker_ignores_sideband_trickle() {
        let t0 = Instant::now();
        let s = |n| t0 + Duration::from_secs(n);
        let mut wire = IdleTracker::new(t0);
        assert_eq!(wire.observe(0, s(2)), Some(WireState::Idle));
        for i in 1..30 {
            assert_eq!(
                wire.observe(i * 60, s(2 + 2 * i)),
                None,
                "progress-line trickle must not un-park the row"
            );
        }
        assert_eq!(
            wire.observe(30 * 60 + NET_ACTIVE_BYTES, s(62)),
            Some(WireState::Active),
            "the pack stream proper un-parks it"
        );
    }

    /// The converse of the trickle test: a stream that degrades into
    /// sub-threshold trickle parks even though the counter never stops
    /// moving (the old any-byte logic would have kept it active forever).
    #[test]
    fn idle_tracker_parks_when_stream_degrades_to_trickle() {
        let t0 = Instant::now();
        let ms = |n| t0 + Duration::from_millis(n);
        let mut wire = IdleTracker::new(t0);
        assert_eq!(wire.observe(NET_ACTIVE_BYTES, ms(120)), None, "streaming");
        assert_eq!(
            wire.observe(NET_ACTIVE_BYTES + 60, ms(1_200)),
            None,
            "trickle, but the window still holds the burst"
        );
        assert_eq!(
            wire.observe(NET_ACTIVE_BYTES + 120, ms(2_400)),
            Some(WireState::Idle),
            "burst aged out; trickle alone is idle"
        );
    }

    /// gix's "read pack … done" message is the last wire event of a fetch —
    /// everything after it is local. The network row must come down right
    /// there, not linger (with a decaying rate) through the ref-update tail.
    #[test]
    fn read_pack_done_clears_the_network_row() {
        let mut root = GixProgress::new("test");
        let child = root.add_child("read pack");
        child.message(MessageLevel::Info, "done. 2.0GiB received".into());
        assert!(
            root.shared.net.handle.lock().unwrap().is_none(),
            "pump should be stopped once the pack is fully read"
        );
        root.finish();
    }

    #[test]
    fn dropping_a_child_keeps_the_pump_alive() {
        // Children share the parent's `Shared` (and thus the one `NetMeter`);
        // gix creates and drops child nodes per phase. A child's `Drop` must
        // clear only its own leaf, never stop the shared pump — otherwise the
        // network row would freeze after the first phase ends.
        let mut root = GixProgress::new("test");
        let counter = root.net_counter();
        drop(root.add_child("phase"));
        counter.fetch_add(2048, Ordering::Relaxed);
        assert_eq!(wait_for_position(&root.shared.net.bar, 2048), 2048);
        root.finish();
    }
}
