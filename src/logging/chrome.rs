//! Custom OpenTelemetry [`SpanExporter`] that writes the Chrome/Perfetto
//! trace-event JSON gitaur has always produced.
//!
//! It's fed from OTEL [`SpanData`] (which carries *explicit* start/end
//! timestamps) instead of `tracing-chrome`, which could only stamp a span at
//! its open/close wall-clock. Two things the switch buys:
//! - the `http request` spans (emitted by the gix curl worker with curl's
//!   CURLINFO timing recorded in `ttfb_ms`/`total_ms`) get `before first byte`
//!   / `after first byte` child slices synthesized here from *backdated*
//!   timestamps — impossible with a stamp-on-close sink.
//! - spans that are held-but-never-entered (the gix fetch-phase progress spans,
//!   see [`crate::ui::gix_progress`]) carry real start/end times, so we no
//!   longer depend on `tracing-chrome`'s fragile Async-style LIFO id-nesting.
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

        // Break an `http request` into its waiting/receiving halves using curl's
        // own TTFB vs. total timing. Same tid → nests under the parent by
        // containment in the Perfetto/chrome view.
        if span.name == HTTP_REQUEST
            && let (Some(ttfb_ms), Some(total_ms)) =
                (attr_i64(span, "ttfb_ms"), attr_i64(span, "total_ms"))
        {
            let ttfb = micros_of(Duration::from_millis(ttfb_ms.max(0).unsigned_abs()));
            let total = micros_of(Duration::from_millis(total_ms.max(0).unsigned_abs()));
            events.push(complete_event(BEFORE_FIRST_BYTE, ts, ttfb, tid, Json::Null));
            events.push(complete_event(
                AFTER_FIRST_BYTE,
                ts + ttfb,
                total.saturating_sub(ttfb),
                tid,
                Json::Null,
            ));
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
    use std::io::Read;

    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use serde_json::Value;
    use tempfile::tempdir;
    use tracing::field::Empty;
    use tracing::subscriber::with_default;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::{EnvFilter, Layer};

    use super::ChromeExporter;

    /// Emit a real `http request` span through the OTEL bridge (the gix worker's
    /// exact shape: `ttfb_ms`/`total_ms` recorded as `u64`) and assert the
    /// exporter synthesizes the two first-byte child slices with the right
    /// timing. This rides the full bridge, so it also guards the `u64`→string
    /// attribute encoding that [`super::attr_i64`] has to tolerate.
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
            // Same recording path as the gix curl worker: CURLINFO as u64.
            span.record("ttfb_ms", 100_u64);
            span.record("total_ms", 250_u64);
            drop(entered);
        });

        provider.shutdown().unwrap();

        let mut json = String::new();
        File::open(&path)
            .unwrap()
            .read_to_string(&mut json)
            .unwrap();
        let doc: Value = serde_json::from_str(&json).unwrap();
        let spans: Vec<&Value> = doc["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "X")
            .collect();

        let parent = spans.iter().find(|e| e["name"] == "http request").unwrap();
        let (p_ts, p_tid) = (
            parent["ts"].as_u64().unwrap(),
            parent["tid"].as_i64().unwrap(),
        );

        let before = spans
            .iter()
            .find(|e| e["name"] == "before first byte")
            .expect("before-first-byte child missing");
        let after = spans
            .iter()
            .find(|e| e["name"] == "after first byte")
            .expect("after-first-byte child missing");

        // Synthesized from CURLINFO: waiting = ttfb (100ms), receiving =
        // total − ttfb (150ms); both on the parent's track, the receiving half
        // starting exactly where the waiting half ends.
        assert_eq!(before["ts"].as_u64().unwrap(), p_ts);
        assert_eq!(before["dur"].as_u64().unwrap(), 100_000);
        assert_eq!(after["ts"].as_u64().unwrap(), p_ts + 100_000);
        assert_eq!(after["dur"].as_u64().unwrap(), 150_000);
        assert_eq!(before["tid"].as_i64().unwrap(), p_tid);
        assert_eq!(after["tid"].as_i64().unwrap(), p_tid);
    }
}
