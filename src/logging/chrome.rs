//! Custom OpenTelemetry [`SpanExporter`] that writes the Chrome/Perfetto
//! trace-event JSON aurox has always produced.
//!
//! It's fed from OTEL [`SpanData`] (which carries *explicit* start/end
//! timestamps) instead of `tracing-chrome`, which could only stamp a span at
//! its open/close wall-clock. The win: the `http request` spans (emitted by the
//! gix curl worker with curl's CURLINFO timing recorded in `ttfb_ms`/`total_ms`)
//! get `before first byte` / `after first byte` child slices synthesized here
//! from *backdated* timestamps — impossible with a stamp-on-close sink.
//!
//! Layout: one Chrome `X` (complete) event per span, `pid=1`, `tid` taken from
//! the `thread.id` attribute the `tracing-opentelemetry` bridge records under
//! `with_threads(true)`. `chrome://tracing` / Perfetto nest events by
//! time-containment within each `(pid, tid)` track, so parent/child structure
//! and the parallel rayon index work both render correctly without us tracking
//! the OTEL parent tree explicitly.

use std::collections::BTreeMap;
use std::fs::File;
use std::future::{Future, ready};
use std::io::{self, BufWriter, Write};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use opentelemetry::Value;
use opentelemetry_sdk::error::{OTelSdkError, OTelSdkResult};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use serde_json::{Map, Value as Json, json};

/// Synthetic child slice names for the `http request` breakdown.
const HTTP_REQUEST: &str = "http request";
const BEFORE_FIRST_BYTE: &str = "before first byte";
const AFTER_FIRST_BYTE: &str = "after first byte";

/// Buffers exported spans and writes the whole trace once, on shutdown.
///
/// We can't stream events as spans close: Chrome's `X` events use *relative*
/// timestamps, and the zero point is the earliest span start — not known until
/// every span is in hand. Spans are small and a single run produces a few
/// hundred, so buffering is cheap.
#[derive(Debug)]
pub struct ChromeExporter {
    inner: Mutex<Inner>,
}

#[derive(Debug)]
struct Inner {
    /// Taken (→ `None`) once shutdown has written and closed the file.
    file: Option<BufWriter<File>>,
    spans: Vec<SpanData>,
}

impl ChromeExporter {
    pub fn new(file: File) -> Self {
        Self {
            inner: Mutex::new(Inner {
                file: Some(BufWriter::new(file)),
                spans: Vec::new(),
            }),
        }
    }
}

impl SpanExporter for ChromeExporter {
    fn export(&self, batch: Vec<SpanData>) -> impl Future<Output = OTelSdkResult> + Send {
        // `SimpleSpanProcessor` drives this with `block_on`, one span at a time;
        // we only stash them and return immediately. A poisoned lock means a
        // prior writer panicked — drop the batch rather than cascade.
        if let Ok(mut inner) = self.inner.lock() {
            inner.spans.extend(batch);
        }
        ready(Ok(()))
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> OTelSdkResult {
        let mut inner = self.inner.lock().map_err(|e| {
            OTelSdkError::InternalFailure(format!("trace buffer lock poisoned: {e}"))
        })?;
        // Idempotent: a second shutdown (explicit + Drop) finds no file.
        let Some(mut file) = inner.file.take() else {
            return Ok(());
        };
        write_trace(&mut file, &inner.spans)
            .map_err(|e| OTelSdkError::InternalFailure(format!("writing trace JSON: {e}")))
    }
}

/// Serialize all buffered spans as a Chrome trace-event document.
fn write_trace(file: &mut BufWriter<File>, spans: &[SpanData]) -> io::Result<()> {
    // Relative-timestamp origin: the earliest span start across the run.
    let base = spans
        .iter()
        .map(|s| s.start_time)
        .min()
        .unwrap_or_else(SystemTime::now);

    // tid → thread name, for the `thread_name` metadata events that label tracks.
    let mut thread_names = BTreeMap::<i64, String>::new();
    let mut events = Vec::<Json>::new();

    for span in spans {
        let tid = attr_i64(span, "thread.id").unwrap_or(0);
        if let Some(name) = attr_str(span, "thread.name") {
            thread_names.entry(tid).or_insert_with(|| name.to_owned());
        }
        let ts = micros_between(base, span.start_time);
        let dur = micros_of(
            span.end_time
                .duration_since(span.start_time)
                .unwrap_or_default(),
        );
        events.push(complete_event(&span.name, ts, dur, tid, args_of(span)));

        // Break an `http request` into its waiting/receiving halves at curl's
        // TTFB. Same tid → nests under the parent by containment in the
        // Perfetto/chrome view. Skip a half that rounds to nothing — a
        // zero-length slice is noise, and an empty receiving half means the
        // whole request was first-byte wait.
        if span.name == HTTP_REQUEST
            && let Some(ttfb_ms) = attr_i64(span, "ttfb_ms")
        {
            let ttfb = micros_of(Duration::from_millis(ttfb_ms.max(0).unsigned_abs()));
            let ((b_ts, b_dur), (a_ts, a_dur)) = first_byte_split(ts, dur, ttfb);
            if b_dur > 0 {
                events.push(complete_event(
                    BEFORE_FIRST_BYTE,
                    b_ts,
                    b_dur,
                    tid,
                    Json::Null,
                ));
            }
            if a_dur > 0 {
                events.push(complete_event(
                    AFTER_FIRST_BYTE,
                    a_ts,
                    a_dur,
                    tid,
                    Json::Null,
                ));
            }
        }
    }

    for (tid, name) in thread_names {
        events.push(json!({
            "name": "thread_name",
            "ph": "M",
            "pid": 1,
            "tid": tid,
            "args": { "name": name },
        }));
    }

    let doc = json!({ "traceEvents": events });
    serde_json::to_writer(&mut *file, &doc)?;
    file.flush()
}

/// Split an `http request`'s window into its before/after-first-byte halves.
///
/// `ttfb` is curl's time-to-first-byte; `dur` is the parent slice's *own*
/// measured length — both in microseconds. The waiting half is `[ts, ts+split]`
/// and the receiving half `[ts+split, ts+dur]`, where `split = min(ttfb, dur)`.
/// They tile the parent: `before` shares the parent's start, `after` shares its
/// end, and they meet at the split. That's fine — Perfetto only drops a complete
/// slice that *crosses* another's end (`slice_drop_overlapping_complete_event`);
/// shared boundaries and bare touches nest or pop cleanly. The split is clamped
/// to the parent's own `dur` (never curl's ms-rounded `total_ms`, a different
/// clock) precisely so `after` can't run past the parent's end and cross it.
///
/// The receiving half is zero-length when `ttfb >= dur`; the caller skips it.
/// Returns `((before_ts, before_dur), (after_ts, after_dur))`.
fn first_byte_split(ts: u64, dur: u64, ttfb: u64) -> ((u64, u64), (u64, u64)) {
    let split = ttfb.min(dur);
    ((ts, split), (ts + split, dur - split))
}

/// One Chrome `X` (complete) event. `ts`/`dur` are microseconds.
fn complete_event(name: &str, ts: u64, dur: u64, tid: i64, args: Json) -> Json {
    let mut event = Map::new();
    event.insert("name".to_owned(), Json::from(name));
    event.insert("ph".to_owned(), Json::from("X"));
    event.insert("ts".to_owned(), Json::from(ts));
    event.insert("dur".to_owned(), Json::from(dur));
    event.insert("pid".to_owned(), Json::from(1));
    event.insert("tid".to_owned(), Json::from(tid));
    if !args.is_null() {
        event.insert("args".to_owned(), args);
    }
    Json::Object(event)
}

/// Fold a span's OTEL attributes into a JSON object for the Perfetto detail
/// pane. Mirrors `tracing-chrome`'s `include_args(true)`.
fn args_of(span: &SpanData) -> Json {
    if span.attributes.is_empty() {
        return Json::Null;
    }
    let mut map = Map::new();
    for kv in &span.attributes {
        map.insert(kv.key.as_str().to_owned(), value_to_json(&kv.value));
    }
    Json::Object(map)
}

fn value_to_json(value: &Value) -> Json {
    match value {
        Value::Bool(b) => Json::Bool(*b),
        Value::I64(i) => Json::from(*i),
        Value::F64(f) => Json::from(*f),
        Value::String(s) => Json::from(s.as_str()),
        // Arrays don't occur in our spans; render via Display rather than grow
        // the match for a case we never emit.
        other => Json::from(other.to_string()),
    }
}

/// First integer-valued attribute matching `key`.
///
/// Accepts both `I64` and an integer-valued `String`: the gix worker records
/// the CURLINFO fields as `u64`, and `tracing-opentelemetry` has no `record_u64`
/// hook, so those fall through to `record_debug` and arrive here stringified
/// (`"287"`). `i64`/`thread.id`-style fields arrive as real `I64`.
fn attr_i64(span: &SpanData, key: &str) -> Option<i64> {
    span.attributes.iter().find_map(|kv| {
        if kv.key.as_str() != key {
            return None;
        }
        match &kv.value {
            Value::I64(v) => Some(*v),
            Value::String(s) => s.as_str().parse().ok(),
            _ => None,
        }
    })
}

/// First string-valued attribute matching `key`.
fn attr_str<'a>(span: &'a SpanData, key: &str) -> Option<&'a str> {
    span.attributes
        .iter()
        .find_map(|kv| match (kv.key.as_str(), &kv.value) {
            (k, Value::String(v)) if k == key => Some(v.as_str()),
            _ => None,
        })
}

fn micros_between(base: SystemTime, t: SystemTime) -> u64 {
    micros_of(t.duration_since(base).unwrap_or_default())
}

fn micros_of(d: Duration) -> u64 {
    u64::try_from(d.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::time::Duration;

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tempfile::tempdir;

    use crate::trace;
    use tracing::field::Empty;
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::{EnvFilter, Layer};

    use super::ChromeExporter;

    /// Emit a real `http request` span through the OTEL bridge (the gix worker's
    /// exact shape: `ttfb_ms`/`total_ms` recorded as `u64`) and assert the
    /// exporter synthesizes the first-byte breakdown *contained* within the
    /// parent. The bridge span lasts only microseconds, so the 100 ms `ttfb_ms`
    /// always exceeds it: this exercises the clamp ([`first_byte_split`] folds the
    /// overrun back to the parent's end), leaving an empty receiving half that's
    /// `slice_drop_overlapping_complete_event`. Riding the full bridge also guards
    /// the `u64`→string attribute encoding that [`super::attr_i64`] has to
    /// tolerate.
    ///
    /// The span sleeps 10 ms with `ttfb_ms = 1`, so the waiting half (1 ms) sits
    /// well inside the parent and both children are emitted — the real two-phase
    /// shape. The headline assertion runs the result back through the reader's
    /// [`crate::trace::overlaps`] (Perfetto's own rule) and demands zero drops,
    /// guarding every boundary trim at once: drop any of them and this fails.
    #[test]
    fn http_request_span_gets_first_byte_children() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("trace.json");

        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(ChromeExporter::new(File::create(&path).unwrap()))
            .build();
        let layer = tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("test"))
            .with_threads(true)
            .with_filter(EnvFilter::new("debug"));
        let subscriber = tracing_subscriber::registry().with(layer);

        with_default(subscriber, || {
            let span = tracing::info_span!(
                "http request",
                method = "GET",
                url = "https://example/info/refs",
                ttfb_ms = Empty,
                total_ms = Empty,
            );
            let entered = span.enter();
            // Same recording path as the gix curl worker: CURLINFO as u64. The
            // sleep makes the parent's measured dur (~10 ms) dwarf ttfb (1 ms),
            // so the split lands inside and both halves are emitted.
            span.record("ttfb_ms", 1_u64);
            span.record("total_ms", 10_u64);
            std::thread::sleep(Duration::from_millis(10));
            drop(entered);
        });

        provider.shutdown().unwrap();

        let events = trace::load(&path).unwrap();
        let over = trace::overlaps(&events);
        assert!(
            over.is_empty(),
            "exporter emitted overlapping slices: {over:?}"
        );

        let span_of = |name: &str| {
            events
                .iter()
                .find(|e| e.name == name)
                .unwrap_or_else(|| panic!("missing {name:?} span"))
        };
        let parent = span_of("http request");
        let before = span_of("before first byte");
        let after = span_of("after first byte");

        // The halves tile the parent: `before` is exactly curl's ttfb (1 ms) and
        // shares the parent's start, `after` runs from the split to the parent's
        // end. Shared boundaries are fine — only a true crossing is dropped, and
        // the overlaps() check above already confirmed there is none.
        assert_eq!(before.dur, 1_000);
        assert_eq!(before.ts, parent.ts);
        assert_eq!(before.tid, parent.tid);
        assert_eq!(after.ts, before.ts + before.dur);
        assert_eq!(after.ts + after.dur, parent.ts + parent.dur);
    }

    /// `ttfb` inside the parent window: `before` is exactly `ttfb`, `after` is the
    /// remainder; together they tile `[1000, 1250)`.
    #[test]
    fn first_byte_split_divides_at_ttfb() {
        assert_eq!(
            super::first_byte_split(1000, 250, 100),
            ((1000, 100), (1100, 150)),
        );
    }

    /// curl's ms-rounded `ttfb` exceeds the parent's measured `dur`: clamp the
    /// split to `dur` so `before` fills the parent and the receiving half is empty
    /// (the caller drops it), rather than `after` running past the parent's end
    /// and crossing it.
    #[test]
    fn first_byte_split_clamps_overrun_to_parent_end() {
        assert_eq!(
            super::first_byte_split(1000, 80, 100),
            ((1000, 80), (1080, 0)),
        );
    }

    /// The synthesized halves must survive Perfetto's stacking — feed them through
    /// the reader's [`crate::trace::overlaps`] (the same rule Perfetto applies) and
    /// assert nothing is dropped. Guards the `dur` clamp: build from `total_ms`
    /// instead and `after` would cross the parent's end and this would fail.
    #[test]
    fn first_byte_split_children_are_overlap_free() {
        let (ts, dur, ttfb) = (1_000_u64, 250_u64, 100_u64);
        let ((b_ts, b_dur), (a_ts, a_dur)) = super::first_byte_split(ts, dur, ttfb);
        let mk = |name: &str, ts, dur| trace::Event {
            name: name.to_owned(),
            tid: 7,
            ts,
            dur,
            args: serde_json::Map::new(),
        };
        let evs = vec![
            mk("http request", ts, dur),
            mk("before first byte", b_ts, b_dur),
            mk("after first byte", a_ts, a_dur),
        ];
        let over = trace::overlaps(&evs);
        assert!(over.is_empty(), "synthesized children overlap: {over:?}");
    }
}
