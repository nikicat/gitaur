//! Progress bars and spinners built on `indicatif`. See the module-level
//! docs on [`super`] for the `{prefix}` vs `{msg}` convention.

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::time::Duration;

/// Tick frames used by every spinner row in this module.
pub(super) const SPIN_TICKS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ";

/// Standard cadence for `enable_steady_tick`.
pub const TICK_PERIOD: Duration = Duration::from_millis(80);

/// Enable a steady tick at the canonical cadence.
///
/// Always call this **after** `MultiProgress::add(pb)` so the tick thread
/// targets the `MultiProgress` draw target — calling it before `add`
/// produces phantom duplicate rows.
pub fn tick(pb: &ProgressBar) {
    pb.enable_steady_tick(TICK_PERIOD);
}

/// Bounded-byte progress bar (used when a total is known up-front).
pub fn bar_bytes(total: u64, label: &str) -> ProgressBar {
    let pb = ProgressBar::new(total);
    pb.set_draw_target(ProgressDrawTarget::hidden());
    pb.set_style(bytes_active_style());
    pb.set_prefix(label.to_owned());
    pb
}

/// Streaming byte counter with no known total (shows `received` + rate +
/// elapsed). Caller should `mp.add(pb)` then `ui::tick(&pb)`.
pub fn bar_bytes_streaming(label: &str) -> ProgressBar {
    let pb = ProgressBar::no_length();
    pb.set_draw_target(ProgressDrawTarget::hidden());
    pb.set_style(bytes_pending_style());
    pb.set_prefix(label.to_owned());
    pb
}

/// Swap a pending byte bar (`bar_bytes_streaming`) to its active form
/// (`bar_bytes`) once a total becomes known.
pub fn promote_byte_bar(pb: &ProgressBar, total: u64) {
    if pb.length() != Some(total) {
        pb.set_length(total);
        pb.set_style(bytes_active_style());
    }
}

/// Park a streaming byte bar (`bar_bytes_streaming`) in its idle form,
/// replacing the rate field with a static `(idle)` tag.
///
/// Once the wire goes quiet the windowed rate no longer measures anything —
/// indicatif keeps re-weighting the estimate by elapsed time, so the shown
/// speed *decays* toward zero over ~15 s, which reads as the transfer
/// regressing. Callers that can detect the lull should park the bar here;
/// [`resume_byte_bar`] restores the live style.
pub(super) fn idle_byte_bar(pb: &ProgressBar) {
    pb.set_style(bytes_idle_style());
}

/// Undo [`idle_byte_bar`]: restore the live streaming style (rate visible).
/// Restarts the rate estimator first — it was measuring the pre-idle
/// transfer, and left alone the resumed row would open on that stale value
/// decaying, not the fresh transfer's speed.
pub(super) fn resume_byte_bar(pb: &ProgressBar) {
    pb.reset_eta();
    pb.set_style(bytes_pending_style());
}

fn bytes_pending_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix:>14.cyan.bold} {spinner} [{elapsed:>4}] {bytes:>10} ({binary_bytes_per_sec})",
    )
    .unwrap()
    .tick_chars(SPIN_TICKS)
}

fn bytes_idle_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix:>14.cyan.bold} {spinner} [{elapsed:>4}] {bytes:>10} (idle)",
    )
    .unwrap()
    .tick_chars(SPIN_TICKS)
}

fn bytes_active_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix:>14.cyan.bold} {spinner} [{elapsed:>4}] {bytes:>10}/{total_bytes:<10} [{bar:20.green/dim}] {binary_bytes_per_sec} (eta {eta})",
    )
    .unwrap()
    .tick_chars(SPIN_TICKS)
    .progress_chars("##-")
}

/// Build a count-oriented progress bar (e.g. parallel index workers).
///
/// When `total == 0`, renders as a spinner-counter (correct UX while we
/// wait for the real total). Call [`promote_count_bar`] once you learn the
/// length to switch to a true progress bar. Caller should `mp.add(pb)`
/// then `ui::tick(&pb)` if the bar starts in pending mode.
pub fn bar_count(total: u64, label: &str) -> ProgressBar {
    let pb = if total == 0 {
        let pb = ProgressBar::no_length();
        pb.set_draw_target(ProgressDrawTarget::hidden());
        pb.set_style(count_pending_style());
        pb
    } else {
        let pb = ProgressBar::new(total);
        pb.set_draw_target(ProgressDrawTarget::hidden());
        pb.set_style(count_active_style());
        pb
    };
    pb.set_prefix(label.to_owned());
    pb
}

/// Swap a pending [`bar_count`] over to the active style and set its length.
/// Idempotent: re-calling with the same total is a no-op.
pub fn promote_count_bar(pb: &ProgressBar, total: u64) {
    if pb.length() != Some(total) {
        pb.set_length(total);
        pb.set_style(count_active_style());
    }
}

fn count_pending_style() -> ProgressStyle {
    // `{elapsed}` runs from the bar's creation; reassures the user that work
    // is happening even when gix doesn't emit per-step events for this phase.
    ProgressStyle::with_template("{prefix:>14.cyan.bold} {spinner} [{elapsed:>4}] {pos:>10}")
        .unwrap()
        .tick_chars(SPIN_TICKS)
}

fn count_active_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{prefix:>14.cyan.bold} {spinner} [{elapsed:>4}] {pos:>10}/{len:<10} [{bar:20.green/dim}] (eta {eta})",
    )
    .unwrap()
    .tick_chars(SPIN_TICKS)
    .progress_chars("##-")
}

/// Spinner with a fixed label, elapsed-time indicator, and streaming `wide_msg` body.
///
/// Used for the libgit2 sideband channel (server-side `remote: Counting
/// objects...` etc.) and other long-running phases.
///
/// Caller should `mp.add(pb)` then `ui::tick(&pb)` so the spinner animates.
pub fn bar_sideband(label: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_draw_target(ProgressDrawTarget::hidden());
    pb.set_style(
        ProgressStyle::with_template("{prefix:>14.cyan.bold} {spinner} [{elapsed:>4}] {wide_msg}")
            .unwrap()
            .tick_chars(SPIN_TICKS),
    );
    pb.set_prefix(label.to_owned());
    pb
}

/// Generic tick spinner for unbounded indeterminate work.
pub fn spinner(label: &str) -> ProgressBar {
    bar_sideband(label)
}
