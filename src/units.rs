//! Typed quantity values: byte sizes and unix timestamps.
//!
//! Same rationale as the name wrappers in [`crate::names`]: a bare `u64`
//! byte count and a bare `i64` timestamp are both "just integers" to the
//! compiler, so they cross-pass silently — a size where an age was meant,
//! seconds where bytes were meant. The wrappers make the unit part of the
//! type and centralize the one human rendering each quantity has.

use jiff::Timestamp;
use jiff::tz::TimeZone;
use rkyv::{Archive, Deserialize, Serialize};
use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A size in bytes (a package's `isize`, a download size).
///
/// `Display` renders the human form (`12.00 MiB`) — the only way aurox ever
/// shows a size to a user, so the formatting lives on the type rather than
/// in a helper each call site must remember.
#[derive(Debug, Clone, Copy, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(u64);

impl ByteSize {
    /// The zero size — alpm's answer for e.g. the download size of a repo
    /// package whose file is already in the pacman cache. A real value, but
    /// info blocks omit it rather than print a misleading `0 B`.
    pub const ZERO: Self = Self(0);

    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
        // f64 precision loss starts petabytes beyond any package size.
        #[allow(clippy::cast_precision_loss)]
        let mut size = self.0 as f64;
        let mut unit = 0;
        while size >= 1024.0 && unit < UNITS.len() - 1 {
            size /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            f.pad(&format!("{} B", self.0))
        } else {
            f.pad(&format!("{size:.2} {}", UNITS[unit]))
        }
    }
}

/// Seconds since the Unix epoch, as git and alpm stamp metadata.
///
/// `0` and below is the "unknown" sentinel throughout (index entries built
/// before the field existed, unreadable commit times) — [`Self::known`]
/// filters it, [`Self::render`] shows it as absent. rkyv-derived because
/// it's archived in the on-disk index (`IndexEntry::commit_time`).
#[derive(
    Archive,
    Serialize,
    Deserialize,
    Debug,
    Clone,
    Copy,
    Default,
    Hash,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
)]
pub struct UnixTime(i64);

impl UnixTime {
    /// Older than any real timestamp — the "never" anchor for freshness
    /// comparisons (e.g. repo rows in the search ranking, which have no
    /// commit of their own). Well below the `≤ 0` unknown sentinel, so it
    /// also renders as absent.
    pub const MIN: Self = Self(i64::MIN);

    pub const fn new(secs: i64) -> Self {
        Self(secs)
    }

    /// The raw seconds — the sanctioned escape hatch for arithmetic at
    /// `SystemTime` / sort-key boundaries.
    pub const fn seconds(self) -> i64 {
        self.0
    }

    /// `None` when the value is the ≤ 0 "unknown" sentinel.
    pub fn known(self) -> Option<Self> {
        (self.0 > 0).then_some(self)
    }

    /// This instant as a [`SystemTime`], for age arithmetic against a wall
    /// clock. `None` for the unknown sentinel.
    pub fn system_time(self) -> Option<SystemTime> {
        let secs = u64::try_from(self.known()?.0).ok()?;
        Some(UNIX_EPOCH + Duration::from_secs(secs))
    }

    /// Render in the system timezone, pacman `-Si` style
    /// (`Sun 13 Jul 2026 14:22:11 EEST`). `None` for the unknown sentinel.
    pub fn render(self) -> Option<String> {
        self.render_in(TimeZone::system())
    }

    fn render_in(self, tz: TimeZone) -> Option<String> {
        let ts = Timestamp::from_second(self.known()?.0).ok()?;
        Some(
            ts.to_zoned(tz)
                .strftime("%a %d %b %Y %H:%M:%S %Z")
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_size_picks_unit_and_precision() {
        assert_eq!(ByteSize::new(0).to_string(), "0 B");
        assert_eq!(ByteSize::new(512).to_string(), "512 B");
        assert_eq!(ByteSize::new(1023).to_string(), "1023 B");
        assert_eq!(ByteSize::new(1024).to_string(), "1.00 KiB");
        assert_eq!(ByteSize::new(1536).to_string(), "1.50 KiB");
        assert_eq!(ByteSize::new(12 * 1024 * 1024).to_string(), "12.00 MiB");
        assert_eq!(
            ByteSize::new(3 * 1024 * 1024 * 1024).to_string(),
            "3.00 GiB"
        );
    }

    #[test]
    fn unix_time_renders_pacman_style() {
        // A fixed zone (not the host's) keeps the assertion hermetic.
        assert_eq!(
            UnixTime::new(1_700_000_000)
                .render_in(TimeZone::UTC)
                .as_deref(),
            Some("Tue 14 Nov 2023 22:13:20 UTC")
        );
    }

    #[test]
    fn unix_time_sentinel_is_unknown() {
        assert_eq!(UnixTime::new(0).known(), None);
        assert_eq!(UnixTime::new(-5).known(), None);
        assert_eq!(UnixTime::new(0).render_in(TimeZone::UTC), None);
        assert_eq!(UnixTime::new(1).known(), Some(UnixTime::new(1)));
    }

    #[test]
    fn unix_time_converts_to_system_time() {
        assert_eq!(
            UnixTime::new(1_700_000_000).system_time(),
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
        assert_eq!(UnixTime::new(0).system_time(), None);
        assert_eq!(UnixTime::MIN.system_time(), None);
    }
}
