//! Read-side analysis of the Chrome/Perfetto span traces gitaur writes to
//! `state_dir()/traces/` (see [`crate::logging::chrome`]).
//!
//! The recurring profiling question — "where does the time inside span X go?" —
//! always needs the same two steps the trace-event JSON doesn't give you
//! directly: reconstruct the parent/child structure (the file is a flat list of
//! `X` events that Perfetto nests purely by time-containment within a track),
//! and turn each span's wall time into a *self* time by subtracting its
//! children. This module does both once, with tests, so the `gitaur-trace`
//! helper can answer the question without a throwaway script each time.
//!
//! Containment is reconstructed per `tid` exactly as Perfetto renders it: events
//! sorted by start, a span is the parent of the next one that starts before it
//! ends. gitaur's spans are synchronous within a thread, so siblings never
//! overlap and `self = dur − Σ children` is exact.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Map, Value as Json};

use crate::error::{Error, Result};
use crate::paths;

/// One `X` (complete) event from the trace, in microseconds.
///
/// Only the fields the analysis needs are pulled out; `args` keeps the span's
/// recorded attributes (e.g. `find_ms`/`commit_ms` on `mark mappings`) so the
/// tree view can show them inline.
#[derive(Debug, Clone)]
pub struct Event {
    pub name: String,
    pub tid: i64,
    /// Start, microseconds from the trace's zero point.
    pub ts: u64,
    /// Duration, microseconds.
    pub dur: u64,
    pub args: Map<String, Json>,
}

/// Raw trace-event document as serialized by [`crate::logging::chrome`].
#[derive(Deserialize)]
struct Document {
    #[serde(rename = "traceEvents")]
    trace_events: Vec<RawEvent>,
}

/// A single entry in `traceEvents`. Metadata (`ph != "X"`) events are skipped
/// after deserialization; dur-less phases simply have no `dur`.
#[derive(Deserialize)]
struct RawEvent {
    #[serde(default)]
    name: String,
    ph: String,
    #[serde(default)]
    tid: i64,
    #[serde(default)]
    ts: u64,
    #[serde(default)]
    dur: u64,
    #[serde(default)]
    args: Map<String, Json>,
}

/// Newest trace file in `state_dir()/traces/`, by filename.
///
/// Trace files are named `gitaur-<YYYYMMDD>-<HHMMSS>-<pid>.json`, so a plain
/// lexical max over the directory is also the most recent run — no `stat` calls.
pub fn latest_trace() -> Result<PathBuf> {
    let dir = paths::traces_dir();
    let newest = std::fs::read_dir(&dir)
        .map_err(|e| Error::other(format!("read {}: {e}", dir.display())))?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .max();
    newest.ok_or_else(|| Error::other(format!("no trace files in {}", dir.display())))
}

/// Parse a trace file into its complete (`X`) events.
pub fn load(path: &Path) -> Result<Vec<Event>> {
    let bytes =
        std::fs::read(path).map_err(|e| Error::other(format!("read {}: {e}", path.display())))?;
    let doc: Document = serde_json::from_slice(&bytes)
        .map_err(|e| Error::other(format!("parse {}: {e}", path.display())))?;
    Ok(doc
        .trace_events
        .into_iter()
        .filter(|e| e.ph == "X")
        .map(|e| Event {
            name: e.name,
            tid: e.tid,
            ts: e.ts,
            dur: e.dur,
            args: e.args,
        })
        .collect())
}

/// A span with its time-contained children, owned for easy rendering.
#[derive(Debug)]
pub struct Node {
    pub name: String,
    pub ts: u64,
    pub dur: u64,
    pub args: Map<String, Json>,
    pub children: Vec<Self>,
}

impl Node {
    const fn end(&self) -> u64 {
        self.ts.saturating_add(self.dur)
    }

    /// Wall time minus the time accounted for by direct children. Siblings in a
    /// thread don't overlap, so this is the span's own (un-instrumented) cost.
    pub fn self_us(&self) -> u64 {
        let children: u64 = self.children.iter().map(|c| c.dur).sum();
        self.dur.saturating_sub(children)
    }
}

/// Reconstruct the per-thread containment forest from a flat event list.
///
/// Mirrors Perfetto's nesting rule: within one `tid`, sort by start time and a
/// span parents whatever begins before it ends. Distinct `tid`s never nest into
/// each other, so the result is a forest (e.g. the main thread and each rayon
/// index worker each contribute roots).
pub fn build_forest(events: &[Event]) -> Vec<Node> {
    let mut by_tid: BTreeMap<i64, Vec<&Event>> = BTreeMap::new();
    for e in events {
        by_tid.entry(e.tid).or_default().push(e);
    }

    let mut roots = Vec::new();
    for (_tid, mut evs) in by_tid {
        // Outer-before-inner: earlier start first, and on a tie the longer span
        // (the would-be parent) first.
        evs.sort_by(|a, b| a.ts.cmp(&b.ts).then(b.dur.cmp(&a.dur)));

        // Open spans, outermost at the bottom. Closing a span attaches it to the
        // new top (its parent) or to `roots` if the stack empties.
        let mut stack: Vec<Node> = Vec::new();
        for e in evs {
            while stack.last().is_some_and(|top| top.end() <= e.ts) {
                close_one(&mut stack, &mut roots);
            }
            stack.push(Node {
                name: e.name.clone(),
                ts: e.ts,
                dur: e.dur,
                args: e.args.clone(),
                children: Vec::new(),
            });
        }
        while !stack.is_empty() {
            close_one(&mut stack, &mut roots);
        }
    }

    // Siblings were attached in finish (reverse-start) order; restore start order
    // everywhere so the tree reads top-to-bottom chronologically.
    for r in &mut roots {
        sort_children(r);
    }
    roots.sort_by_key(|n| (n.ts, std::cmp::Reverse(n.dur)));
    roots
}

/// Pop the innermost open span and attach it under its parent (or as a root).
fn close_one(stack: &mut Vec<Node>, roots: &mut Vec<Node>) {
    let Some(done) = stack.pop() else { return };
    match stack.last_mut() {
        Some(parent) => parent.children.push(done),
        None => roots.push(done),
    }
}

fn sort_children(node: &mut Node) {
    node.children
        .sort_by_key(|c| (c.ts, std::cmp::Reverse(c.dur)));
    for c in &mut node.children {
        sort_children(c);
    }
}

/// A complete slice Perfetto drops because it *crosses* an open slice's end.
///
/// `dropped` begins inside `over` but ends after it (`over.start ≤ dropped.start
/// < over.end < dropped.end`) — a partial overlap, which on one track is invalid
/// and discarded as `slice_drop_overlapping_complete_event`. Note what is *not*
/// here: slices that merely share a start, share an end, or touch are all fine —
/// Perfetto nests or pops them. Only a true crossing is dropped.
#[derive(Debug, Clone)]
pub struct Overlap {
    pub tid: i64,
    /// The slice that escapes its container — the one Perfetto discards.
    pub dropped: Event,
    /// The open slice whose end it crosses.
    pub over: Event,
}

/// Find the complete slices Perfetto drops, per track.
///
/// Faithfully replays Perfetto's `SliceTracker::Scoped` (see
/// `slice_tracker.cc`): process each `X` slice in `(timestamp, file order)`,
/// maintaining a per-track stack. Before placing a new slice, pop every open one
/// that ended at or before it starts (a touch *pops* — it is not an overlap),
/// then the innermost still-open slice is the parent. A new slice is dropped
/// only on a true crossing — it starts before that parent's end yet ends after
/// it (`new.start < end < new.end`); otherwise it nests.
///
/// This is deliberately *not* a "strictly nested or disjoint" check: shared
/// starts, shared ends, and touches do not drop in real Perfetto. Earlier
/// attempts that guessed a begin/end split with a same-timestamp tiebreak (or
/// flagged every shared boundary) produced both false negatives and false
/// positives; this mirrors the engine instead.
pub fn overlaps(events: &[Event]) -> Vec<Overlap> {
    let mut by_tid: BTreeMap<i64, Vec<&Event>> = BTreeMap::new();
    for e in events {
        by_tid.entry(e.tid).or_default().push(e);
    }

    let mut out = Vec::new();
    for (_tid, mut evs) in by_tid {
        // Perfetto sorts by timestamp; file order is the stable tiebreak. `evs`
        // is already in file order, so a stable sort by `ts` reproduces it.
        evs.sort_by_key(|e| e.ts);
        let mut stack: Vec<&Event> = Vec::new();
        for e in evs {
            let new_ts = e.ts;
            let new_end = e.ts.saturating_add(e.dur);
            let mut dropped = false;
            while let Some(top) = stack.last() {
                let end = top.ts.saturating_add(top.dur);
                // Pop slices that ended before `e`, or exactly at its start —
                // unless both are zero-length instants (those coexist).
                if end < new_ts || (end == new_ts && !(top.dur == 0 && e.dur == 0)) {
                    stack.pop();
                    continue;
                }
                // `top` is still open. Only a true crossing is dropped.
                if new_ts < end && new_end > end {
                    out.push(Overlap {
                        tid: e.tid,
                        dropped: e.clone(),
                        over: (*top).clone(),
                    });
                    dropped = true;
                }
                break; // nested in `top` (or dropped): stop here either way.
            }
            if !dropped {
                stack.push(e);
            }
        }
    }
    out
}

/// Per-name aggregate across the whole trace.
#[derive(Debug, Clone)]
pub struct Agg {
    pub name: String,
    pub count: u64,
    /// Σ wall time over every occurrence, microseconds.
    pub total_us: u64,
    /// Σ self time over every occurrence, microseconds.
    pub self_us: u64,
    /// Longest single occurrence, microseconds.
    pub max_us: u64,
}

/// Aggregate the forest by span name, sorted by total self time descending —
/// the spans that actually burned wall clock float to the top.
pub fn summarize(roots: &[Node]) -> Vec<Agg> {
    let mut by_name: BTreeMap<&str, Agg> = BTreeMap::new();
    collect(roots, &mut by_name);
    let mut out: Vec<Agg> = by_name.into_values().collect();
    out.sort_by(|a, b| {
        b.self_us
            .cmp(&a.self_us)
            .then(b.total_us.cmp(&a.total_us))
            .then(a.name.cmp(&b.name))
    });
    out
}

fn collect<'a>(nodes: &'a [Node], by_name: &mut BTreeMap<&'a str, Agg>) {
    for n in nodes {
        let agg = by_name.entry(n.name.as_str()).or_insert_with(|| Agg {
            name: n.name.clone(),
            count: 0,
            total_us: 0,
            self_us: 0,
            max_us: 0,
        });
        agg.count += 1;
        agg.total_us += n.dur;
        agg.self_us += n.self_us();
        agg.max_us = agg.max_us.max(n.dur);
        collect(&n.children, by_name);
    }
}

/// Collect references to every node named `name`, depth-first, for the
/// `--span`-rooted tree view. There can be several (e.g. one `negotiate round`
/// per round) — each is returned as its own subtree root.
pub fn find_by_name<'a>(roots: &'a [Node], name: &str) -> Vec<&'a Node> {
    fn walk<'a>(nodes: &'a [Node], name: &str, hits: &mut Vec<&'a Node>) {
        for n in nodes {
            if n.name == name {
                hits.push(n);
            }
            walk(&n.children, name, hits);
        }
    }
    let mut hits = Vec::new();
    walk(roots, name, &mut hits);
    hits
}

/// Human-readable microsecond duration: `µs` under a millisecond, `ms` under a
/// second, `s` above.
pub fn fmt_dur(us: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    if us >= 1_000_000 {
        format!("{:.2}s", us as f64 / 1_000_000.0)
    } else if us >= 1_000 {
        format!("{:.1}ms", us as f64 / 1_000.0)
    } else {
        format!("{us}µs")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ev(name, tid, ts, dur)` with no args.
    fn ev(name: &str, tid: i64, ts: u64, dur: u64) -> Event {
        Event {
            name: name.to_owned(),
            tid,
            ts,
            dur,
            args: Map::new(),
        }
    }

    #[test]
    fn nests_by_time_containment() {
        // outer[0..100] contains a[10..40] and b[40..90]; a contains a1[15..25].
        let evs = vec![
            ev("outer", 1, 0, 100),
            ev("a", 1, 10, 30),
            ev("a1", 1, 15, 10),
            ev("b", 1, 40, 50),
        ];
        let forest = build_forest(&evs);
        assert_eq!(forest.len(), 1);
        let outer = &forest[0];
        assert_eq!(outer.name, "outer");
        // Direct children in chronological order.
        assert_eq!(
            outer
                .children
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        let a = &outer.children[0];
        assert_eq!(
            a.children
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["a1"]
        );
    }

    #[test]
    fn self_time_excludes_children() {
        let evs = vec![
            ev("outer", 1, 0, 100),
            ev("a", 1, 10, 30),
            ev("b", 1, 40, 50),
        ];
        let forest = build_forest(&evs);
        let outer = &forest[0];
        // 100 total − (30 + 50) children = 20 self.
        assert_eq!(outer.self_us(), 20);
        // Leaf self == dur.
        assert_eq!(outer.children[0].self_us(), 30);
    }

    #[test]
    fn separate_tids_are_separate_roots() {
        let evs = vec![ev("main", 1, 0, 100), ev("worker", 2, 10, 50)];
        let forest = build_forest(&evs);
        assert_eq!(forest.len(), 2);
        // No cross-thread nesting even though worker is inside main's time span.
        assert!(forest.iter().all(|n| n.children.is_empty()));
    }

    #[test]
    fn summarize_sorts_by_self_and_folds_repeats() {
        let evs = vec![
            ev("round", 1, 0, 100),
            ev("io", 1, 0, 90), // self 90
            ev("round", 1, 100, 60),
            ev("io", 1, 100, 10), // self 10
        ];
        let forest = build_forest(&evs);
        let summary = summarize(&forest);
        // io: self 90+10=100; round: self 10+50=60. io leads.
        assert_eq!(summary[0].name, "io");
        assert_eq!(summary[0].count, 2);
        assert_eq!(summary[0].self_us, 100);
        assert_eq!(summary[0].total_us, 100);
        let round = summary.iter().find(|a| a.name == "round").unwrap();
        assert_eq!(round.self_us, 60);
        assert_eq!(round.max_us, 100);
    }

    #[test]
    fn find_by_name_returns_each_occurrence() {
        let evs = vec![
            ev("negotiate", 1, 0, 100),
            ev("round", 1, 0, 40),
            ev("round", 1, 40, 40),
        ];
        let forest = build_forest(&evs);
        let rounds = find_by_name(&forest, "round");
        assert_eq!(rounds.len(), 2);
    }

    #[test]
    fn overlaps_flags_partial_crossing() {
        // X starts inside A [0,100] but ends at 150 — it crosses A's end. This is
        // the only shape Perfetto drops; build_forest would hide it under A.
        let over = overlaps(&[ev("A", 1, 0, 100), ev("X", 1, 80, 70)]);
        assert_eq!(over.len(), 1);
        assert_eq!(over[0].tid, 1);
        assert_eq!(over[0].dropped.name, "X");
        assert_eq!(over[0].over.name, "A");
    }

    #[test]
    fn overlaps_flags_phase_style_crossing() {
        // The real bug shape: an annotation span opens just after an op span and
        // closes after it ends — crossing the op's end. Dropped.
        let over = overlaps(&[ev("op", 1, 0, 100), ev("phase", 1, 10, 100)]);
        assert_eq!(over.len(), 1);
        assert_eq!(over[0].dropped.name, "phase");
        assert_eq!(over[0].over.name, "op");
    }

    #[test]
    fn overlaps_ignores_nesting_touch_and_shared_boundaries() {
        // Everything Perfetto tolerates and that earlier over-strict rules wrongly
        // flagged: a nested child, a child sharing the parent's start, two siblings
        // that touch (`a.end == b.start`), and a child sharing the parent's end.
        let evs = vec![
            ev("outer", 1, 0, 100),
            ev("shares_start", 1, 0, 20), // shared start with outer
            ev("a", 1, 20, 30),           // [20,50]
            ev("b", 1, 50, 30),           // [50,80] touches a
            ev("shares_end", 1, 80, 20),  // [80,100] shares end with outer
        ];
        assert!(overlaps(&evs).is_empty(), "{:?}", overlaps(&evs));
    }

    #[test]
    fn overlaps_are_per_track() {
        // Same wall-clock window, different tids: never an overlap.
        let evs = vec![ev("main", 1, 0, 100), ev("worker", 2, 50, 100)];
        assert!(overlaps(&evs).is_empty());
    }

    #[test]
    fn overlaps_dropped_slice_does_not_blame_later_ones() {
        // X crosses A and is dropped; Y starts after A and must stand alone,
        // not be reported as overlapping the (non-existent) X.
        let evs = vec![ev("A", 1, 0, 100), ev("X", 1, 80, 70), ev("Y", 1, 200, 50)];
        let over = overlaps(&evs);
        assert_eq!(over.len(), 1);
        assert_eq!(over[0].dropped.name, "X");
    }

    #[test]
    fn fmt_dur_picks_a_unit() {
        assert_eq!(fmt_dur(500), "500µs");
        assert_eq!(fmt_dur(1_500), "1.5ms");
        assert_eq!(fmt_dur(2_500_000), "2.50s");
    }
}
