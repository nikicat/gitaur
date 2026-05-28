//! `gitaur-trace` — inspect the per-run span traces gitaur writes to
//! `state_dir()/traces/`.
//!
//! The flat Chrome trace-event JSON renders fine in Perfetto, but answering
//! "where did the time in `receive` go?" from the terminal otherwise meant a
//! throwaway script each time. This wraps [`gitaur::trace`]'s containment-tree
//! and self-time analysis in two views:
//!
//!   gitaur-trace                 # summary: spans by total self time
//!   gitaur-trace tree            # full per-thread containment tree
//!   gitaur-trace tree --span receive   # just the subtree(s) under `receive`
//!
//! With no `--file`, it picks the newest trace in the traces directory.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use gitaur::trace::{self, Agg, Node};

#[derive(Parser)]
#[command(name = "gitaur-trace", about = "Analyze gitaur span traces")]
struct Cli {
    /// Trace file to read (default: newest in the traces directory).
    #[arg(short, long, global = true)]
    file: Option<PathBuf>,

    /// Hide spans shorter than this many milliseconds.
    #[arg(long, global = true, default_value_t = 0)]
    min_ms: u64,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Aggregate every span by name, sorted by total self time (default).
    Summary,
    /// Print the per-thread containment tree.
    Tree {
        /// Root the tree at every span with this exact name instead of the
        /// whole forest (e.g. `--span receive`).
        #[arg(long)]
        span: Option<String>,
        /// Stop descending past this depth (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        depth: usize,
    },
    /// List complete slices Perfetto drops because they overlap a sibling on
    /// the same track (`slice_drop_overlapping_complete_event`).
    Overlaps,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let path = match &cli.file {
        Some(p) => p.clone(),
        None => trace::latest_trace()?,
    };
    let events = trace::load(&path)?;
    let forest = trace::build_forest(&events);
    let over = trace::overlaps(&events);
    let min_us = cli.min_ms.saturating_mul(1_000);
    let cmd = cli.cmd.unwrap_or(Cmd::Summary);

    eprintln!("{} — {} spans", path.display(), events.len());
    // Surface the drop count up front: an overlap means Perfetto silently omits
    // a slice and the tree below mis-attributes its parent's self time.
    if !over.is_empty() && !matches!(cmd, Cmd::Overlaps) {
        eprintln!(
            "  warning: {} overlapping slice(s) Perfetto will drop — run `gitaur-trace overlaps`",
            over.len(),
        );
    }

    match cmd {
        Cmd::Summary => print_summary(&trace::summarize(&forest), min_us),
        Cmd::Overlaps => print_overlaps(&over),
        Cmd::Tree { span, depth } => {
            let depth = if depth == 0 { usize::MAX } else { depth };
            let roots: Vec<&Node> = match &span {
                Some(name) => trace::find_by_name(&forest, name),
                None => forest.iter().collect(),
            };
            if roots.is_empty() {
                anyhow::bail!(
                    "no span named {:?} in {}",
                    span.unwrap_or_default(),
                    path.display()
                );
            }
            for root in roots {
                print_tree(root, 0, depth, min_us);
            }
        }
    }
    Ok(())
}

/// Tabular `self  total  count  name`, widest-first by self time.
fn print_summary(aggs: &[Agg], min_us: u64) {
    println!("{:>10}  {:>10}  {:>5}  name", "self", "total", "count");
    for a in aggs.iter().filter(|a| a.total_us >= min_us) {
        println!(
            "{:>10}  {:>10}  {:>5}  {}",
            trace::fmt_dur(a.self_us),
            trace::fmt_dur(a.total_us),
            a.count,
            a.name,
        );
    }
}

/// List every overlapping slice, naming the slice Perfetto drops and the one it
/// overruns, each with its `[start..end]` window so it's findable in Perfetto.
fn print_overlaps(over: &[trace::Overlap]) {
    if over.is_empty() {
        println!("no overlapping slices — Perfetto renders every complete event");
        return;
    }
    println!(
        "{} overlapping slice(s) dropped as slice_drop_overlapping_complete_event:",
        over.len(),
    );
    let window = |e: &trace::Event| {
        format!(
            "[{}..{}]",
            trace::fmt_dur(e.ts),
            trace::fmt_dur(e.ts.saturating_add(e.dur)),
        )
    };
    for o in over {
        println!(
            "  tid {}: '{}' {} collides with '{}' {}",
            o.tid,
            o.dropped.name,
            window(&o.dropped),
            o.over.name,
            window(&o.over),
        );
    }
}

/// Indented subtree. Shows each span's wall time, its self time when it has
/// children (so un-instrumented gaps are visible), and any recorded args.
fn print_tree(node: &Node, indent: usize, max_depth: usize, min_us: u64) {
    if node.dur < min_us {
        return;
    }
    let pad = "  ".repeat(indent);
    let self_note = if node.children.is_empty() {
        String::new()
    } else {
        format!(" (self {})", trace::fmt_dur(node.self_us()))
    };
    println!(
        "{pad}{} {}{}{}",
        node.name,
        trace::fmt_dur(node.dur),
        self_note,
        fmt_args(node),
    );
    if indent + 1 >= max_depth {
        return;
    }
    for c in &node.children {
        print_tree(c, indent + 1, max_depth, min_us);
    }
}

/// Render a node's recorded attributes as ` {k=v, …}`, or empty when none.
fn fmt_args(node: &Node) -> String {
    if node.args.is_empty() {
        return String::new();
    }
    let body = node
        .args
        .iter()
        .filter(|(k, _)| is_span_field(k))
        .map(|(k, v)| format!("{k}={}", render_json(v)))
        .collect::<Vec<_>>()
        .join(", ");
    if body.is_empty() {
        String::new()
    } else {
        format!("  {{{body}}}")
    }
}

/// Keep the span's own recorded fields, drop the bookkeeping the
/// `tracing-opentelemetry` bridge and chrome exporter attach to every span
/// (`code.*`/`target` source location, `busy_ns`/`idle_ns` timing already shown
/// as the duration, `thread.*` track labels).
fn is_span_field(key: &str) -> bool {
    !(key.starts_with("code.")
        || key.starts_with("thread.")
        || matches!(key, "busy_ns" | "idle_ns" | "target"))
}

/// Compact scalar rendering — strings without their JSON quotes.
fn render_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}
