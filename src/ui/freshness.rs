//! Freshness as a **non-linear trust band** for AUR search rows.
//!
//! yay colors its age tag on a linear scale — hottest = newest — which
//! silently equates "just pushed" with "best." After the 2024–2025 AUR
//! supply-chain incidents (malicious PKGBUILDs pushed to legit-looking
//! packages, noticed by the community within days), that equation is wrong: a
//! pkgbase whose PKGBUILD changed hours ago is one you would build *before*
//! anyone had a chance to vet the change — the exact window an attack lands in.
//! So the last-change age carries **risk at both ends** and trust in the
//! middle, and we render it as four bands rather than a gradient:
//!
//! | Band | Age (default) | Reading | Color |
//! |------|---------------|---------|-------|
//! | [`Caution`](FreshnessBand::Caution)  | `< 2d`      | just changed — *unvetted* | bold yellow ⚠ (draws the eye) |
//! | [`Fresh`](FreshnessBand::Fresh)      | `2d – 180d` | actively maintained, had time to be vetted | green (the trust band) |
//! | [`Maturing`](FreshnessBand::Maturing)| `180d – 730d` | stable/aging, still alive | plain (no flag) |
//! | [`Stale`](FreshnessBand::Stale)      | `> 730d`    | likely abandoned | dim (recedes) |
//!
//! The palette is deliberately *not* a gradient: the two risky bands sit at the
//! extremes and read oppositely — **bright** at the fresh end (attention: new &
//! unvetted), **faded** at the old end (dismissal: probably dead) — while trust
//! is green in the middle.
//!
//! Thresholds are configurable ([`AgeThresholds`], sourced from
//! [`Config::age_thresholds`](crate::config::Config::age_thresholds)); the band
//! *colors* are fixed. Classification ([`FreshnessBand::classify`]) is pure —
//! "now" is captured once at the render boundary and injected via [`AgeScale`],
//! never read inside, so the mapping is deterministic and unit-testable.

use super::grid::{Cell, Paint};
use super::{dim, human_age};
use crate::units::UnixTime;
use console::style;
use std::time::{Duration, SystemTime};

/// Default caution window: under this age a change is too recent to be vetted.
const DEFAULT_AGE_CAUTION_DAYS: u64 = 2;
/// Default upper bound of the "actively maintained" band.
const DEFAULT_AGE_FRESH_DAYS: u64 = 180;
/// Default age past which a pkgbase reads as abandoned.
const DEFAULT_AGE_STALE_DAYS: u64 = 730;

/// Whole days as a [`Duration`] — the one place the day→seconds conversion for
/// the thresholds lives (config knobs are day counts; classification is in
/// `Duration`). `Duration::from_days` is still unstable, so this floors on
/// `from_secs`.
const fn days(n: u64) -> Duration {
    Duration::from_secs(n * 86_400)
}

/// Which trust band a pkgbase's last-change age falls in.
///
/// Variant order runs youngest → oldest, but this is **not** a quality
/// gradient — both [`Caution`](Self::Caution) (too new to vet) and
/// [`Stale`](Self::Stale) (abandoned) are the risky ends; the trust sits in
/// [`Fresh`](Self::Fresh)/[`Maturing`](Self::Maturing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshnessBand {
    /// Changed within the caution window — unvetted; glance at the PKGBUILD diff
    /// before trusting (the supply-chain window).
    Caution,
    /// Actively maintained and old enough to have been community-vetted.
    Fresh,
    /// Stable/aging: not recently touched, but not abandoned.
    Maturing,
    /// Long untouched — likely abandoned, may not build.
    Stale,
}

impl FreshnessBand {
    /// Classify an age against the thresholds. Each boundary belongs to the
    /// *younger* band: `age < caution` → [`Caution`](Self::Caution),
    /// `< fresh` → [`Fresh`](Self::Fresh), `< stale` →
    /// [`Maturing`](Self::Maturing), otherwise [`Stale`](Self::Stale).
    fn classify(age: Duration, t: &AgeThresholds) -> Self {
        if age < t.caution {
            Self::Caution
        } else if age < t.fresh {
            Self::Fresh
        } else if age < t.stale {
            Self::Maturing
        } else {
            Self::Stale
        }
    }

    /// Paint `text` in the band's semantic color; plain paint returns `text`
    /// unchanged. Caution = bold yellow (loud caution), Fresh = green (trust),
    /// Maturing = plain, Stale = dim (recedes).
    fn paint(self, text: &str, paint: Paint) -> String {
        if !paint.colored() {
            return text.to_owned();
        }
        match self {
            Self::Caution => style(text.to_owned()).bold().yellow().to_string(),
            Self::Fresh => style(text.to_owned()).green().to_string(),
            Self::Maturing => text.to_owned(),
            Self::Stale => dim(text).to_string(),
        }
    }

    /// The band as read for a VCS pkgbase (`-git`/`-svn`/…): its freshness is
    /// the *PKGBUILD's* last-change age, but a VCS recipe rebuilds from upstream
    /// HEAD every time — an old PKGBUILD is *stable packaging*, not abandonment.
    /// So [`Stale`](Self::Stale) never applies to a VCS pkgbase; it clamps up to
    /// [`Maturing`](Self::Maturing) ("alive, just not recently repackaged").
    ///
    /// The too-fresh [`Caution`](Self::Caution) end still holds — a PKGBUILD that
    /// changed hours ago is unvetted whether or not upstream is a VCS. A no-op
    /// for a non-VCS package.
    #[must_use]
    const fn vcs_clamped(self, is_vcs: bool) -> Self {
        if is_vcs && matches!(self, Self::Stale) {
            Self::Maturing
        } else {
            self
        }
    }
}

/// The age boundaries between the four [`FreshnessBand`]s, ascending
/// (`caution < fresh < stale`).
///
/// Sourced from config; [`Default`] carries the built-in windows. The
/// day→[`Duration`] conversion lives once in [`Self::from_days`] so the config
/// knobs stay plain day counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgeThresholds {
    /// Below this, a change is too recent to be vetted ([`FreshnessBand::Caution`]).
    pub caution: Duration,
    /// Up to this age counts as actively maintained ([`FreshnessBand::Fresh`]).
    pub fresh: Duration,
    /// Beyond this, treat as abandoned ([`FreshnessBand::Stale`]).
    pub stale: Duration,
}

impl AgeThresholds {
    /// Build from whole-day counts — the shape the config knobs carry; the one
    /// place the day→[`Duration`] conversion for the thresholds lives.
    pub const fn from_days(caution: u64, fresh: u64, stale: u64) -> Self {
        Self {
            caution: days(caution),
            fresh: days(fresh),
            stale: days(stale),
        }
    }

    /// Resolve the sparse `[ages]` config section: a `Some(days)` pins that
    /// band, a `None` follows [`Default`]. The resolver behind
    /// [`Config::age_thresholds`](crate::config::Config::age_thresholds), so the
    /// default day counts stay private to this module.
    pub fn from_day_overrides(
        caution: Option<u64>,
        fresh: Option<u64>,
        stale: Option<u64>,
    ) -> Self {
        let base = Self::default();
        Self {
            caution: caution.map_or(base.caution, days),
            fresh: fresh.map_or(base.fresh, days),
            stale: stale.map_or(base.stale, days),
        }
    }
}

impl Default for AgeThresholds {
    fn default() -> Self {
        Self::from_days(
            DEFAULT_AGE_CAUTION_DAYS,
            DEFAULT_AGE_FRESH_DAYS,
            DEFAULT_AGE_STALE_DAYS,
        )
    }
}

/// A search row's freshness badge: the pkgbase's last-change age plus the
/// [`FreshnessBand`] it lands in. Renders as a band-colored coarse age (`3d`).
///
/// An *absent* badge is `Option::None` on the row (repo packages, or an AUR
/// entry with an unknown/future commit time) — this type never represents
/// "no age."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Freshness {
    age: Duration,
    band: FreshnessBand,
}

impl Freshness {
    /// This badge as a single-line search-table [`Cell`]: the coarse age
    /// (`3d`), colored by risk band. It's its own aligned column, so no brackets
    /// delimit it and the grid supplies the surrounding gap.
    pub fn cell(&self, paint: Paint) -> Cell {
        let text = human_age(self.age);
        Cell::paint(&text, paint, |s| self.band.paint(s, paint))
    }

    /// This badge as an inline **two-line-headline** tag: the coarse age
    /// bracketed (`[3d]`), band-colored. The two-line layout has no freshness
    /// *column* to align, so the brackets delimit the age inline (the opposite
    /// call from [`Self::cell`], whose own column needs none). Whole-tag paint —
    /// the brackets carry the band color too, so a caution `[1m]` reads as one
    /// yellow unit.
    pub fn tag(&self, paint: Paint) -> String {
        self.band
            .paint(&format!("[{}]", human_age(self.age)), paint)
    }

    /// The band this badge fell in.
    pub const fn band(&self) -> FreshnessBand {
        self.band
    }
}

/// Turns an AUR pkgbase's last-commit time into a [`Freshness`] badge.
///
/// Holds "now" — captured **once** at the render boundary via [`Self::now`],
/// never read again — plus the configured [`AgeThresholds`], so a whole search
/// render classifies against one consistent, injectable clock (tests use
/// [`Self::at`]).
#[derive(Debug, Clone, Copy)]
pub struct AgeScale {
    now: SystemTime,
    thresholds: AgeThresholds,
}

impl AgeScale {
    /// Capture the clock once for a search render.
    pub fn now(thresholds: AgeThresholds) -> Self {
        Self {
            now: SystemTime::now(),
            thresholds,
        }
    }

    /// Construct against an explicit "now" — deterministic, for tests.
    pub const fn at(now: SystemTime, thresholds: AgeThresholds) -> Self {
        Self { now, thresholds }
    }

    /// The badge for a commit time, or `None` when it is unknown or in the
    /// future (clock skew) — no badge rather than a bogus zero-age caution.
    ///
    /// `is_vcs` clamps the [`FreshnessBand::Stale`] end away for VCS pkgbases
    /// (`-git`/`-svn`/…), whose old PKGBUILD is stable packaging rather than
    /// abandonment — see [`FreshnessBand::vcs_clamped`]. The same band feeds the
    /// displayed tag and the search ranking, so a VCS row is never shown *or*
    /// sorted as abandoned.
    pub fn badge(&self, commit_time: UnixTime, is_vcs: bool) -> Option<Freshness> {
        let age = self.now.duration_since(commit_time.system_time()?).ok()?;
        Some(Freshness {
            age,
            band: FreshnessBand::classify(age, &self.thresholds).vcs_clamped(is_vcs),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: AgeThresholds = AgeThresholds::from_days(2, 180, 730);

    /// Each boundary belongs to the younger band; the four bands map to the
    /// four age ranges exactly.
    #[test]
    fn classify_maps_ages_to_bands_at_the_boundaries() {
        let band = |secs| FreshnessBand::classify(Duration::from_secs(secs), &T);
        let day = 86_400;
        assert_eq!(band(0), FreshnessBand::Caution);
        assert_eq!(band(day), FreshnessBand::Caution, "1d is still caution");
        assert_eq!(band(2 * day), FreshnessBand::Fresh, "2d flips to fresh");
        assert_eq!(band(179 * day), FreshnessBand::Fresh);
        assert_eq!(
            band(180 * day),
            FreshnessBand::Maturing,
            "180d flips to maturing"
        );
        assert_eq!(band(729 * day), FreshnessBand::Maturing);
        assert_eq!(band(730 * day), FreshnessBand::Stale, "730d flips to stale");
    }

    /// The sparse-override resolver pins a `Some` band and leaves the rest at
    /// their [`Default`] — the `[ages]` config section's semantics.
    #[test]
    fn from_day_overrides_pins_set_and_defaults_unset() {
        let base = AgeThresholds::default();
        let t = AgeThresholds::from_day_overrides(Some(7), None, None);
        assert_eq!(t.caution, days(7), "set key is pinned");
        assert_eq!(t.fresh, base.fresh, "unset key follows default");
        assert_eq!(t.stale, base.stale, "unset key follows default");
        assert_eq!(
            AgeThresholds::from_day_overrides(None, None, None),
            base,
            "all-unset resolves to the default"
        );
    }

    /// A tighter caution window moves the caution→fresh boundary — the knob is
    /// live, not baked.
    #[test]
    fn thresholds_are_configurable() {
        let strict = AgeThresholds::from_days(7, 180, 730);
        let age = days(3);
        assert_eq!(FreshnessBand::classify(age, &T), FreshnessBand::Fresh);
        assert_eq!(
            FreshnessBand::classify(age, &strict),
            FreshnessBand::Caution,
            "3d is caution when the window is 7d"
        );
    }

    /// The scale computes age from an injected `now`; an unknown or future
    /// commit time yields no badge.
    #[test]
    fn scale_badges_known_past_times_only() {
        let epoch = SystemTime::UNIX_EPOCH;
        let now = epoch + days(1_000);
        let scale = AgeScale::at(now, T);

        // 3 days ago → Fresh.
        let three_days_ago = UnixTime::new(997 * 86_400);
        let badge = scale
            .badge(three_days_ago, false)
            .expect("known past time badges");
        assert_eq!(badge.band(), FreshnessBand::Fresh);

        // Future commit (clock skew) → no badge.
        let future = UnixTime::new(1_100 * 86_400);
        assert_eq!(scale.badge(future, false), None, "future commit → no badge");

        // Unknown sentinel (≤ 0) → no badge.
        assert_eq!(scale.badge(UnixTime::new(0), false), None);
    }

    /// A VCS pkgbase never reads as `Stale`: an old `-git` PKGBUILD is stable
    /// packaging, not abandonment, so its stale-age badge clamps up to
    /// `Maturing`. A non-VCS pkgbase of the same age still reads `Stale`; and
    /// the too-fresh `Caution` end is unaffected by the clamp.
    #[test]
    fn vcs_pkgbase_never_reads_stale() {
        let now = SystemTime::UNIX_EPOCH + days(2_000);
        let scale = AgeScale::at(now, T);
        // 1000 days old (> the 730d stale threshold).
        let ancient = UnixTime::new(1_000 * 86_400);
        assert_eq!(
            scale.badge(ancient, false).map(|f| f.band()),
            Some(FreshnessBand::Stale),
            "non-VCS old PKGBUILD is stale"
        );
        assert_eq!(
            scale.badge(ancient, true).map(|f| f.band()),
            Some(FreshnessBand::Maturing),
            "VCS old PKGBUILD clamps stale → maturing"
        );

        // The clamp only touches the stale end — a fresh VCS PKGBUILD is unmoved.
        let recent = UnixTime::new(1_990 * 86_400); // 10 days old → Fresh
        assert_eq!(
            scale.badge(recent, true).map(|f| f.band()),
            Some(FreshnessBand::Fresh),
            "a recently-changed VCS PKGBUILD keeps its band"
        );
    }

    /// Each band paints its tag in the intended style: caution/fresh/stale carry
    /// ANSI, maturing stays bare, and every band strips back to the plain text.
    #[test]
    fn band_paint_colors_by_band() {
        use FreshnessBand::{Caution, Fresh, Maturing, Stale};

        // `console` gates ANSI on its own tty detection at render time; force it
        // on so the colored branch is observable under piped `cargo test`.
        console::set_colors_enabled(true);

        // Plain paint is always the bare text, whatever the band.
        for b in [Caution, Fresh, Maturing, Stale] {
            assert_eq!(b.paint("3d", Paint::Plain), "3d");
        }
        // Colored: the two risky ends and the trust band are styled; maturing
        // (the "no flag" middle) stays uncolored.
        for b in [Caution, Fresh, Stale] {
            let c = b.paint("3d", Paint::Colored);
            assert!(c.contains('\u{1b}'), "{b:?} carries ANSI: {c:?}");
            assert_eq!(console::strip_ansi_codes(&c), "3d");
        }
        assert_eq!(
            Maturing.paint("3d", Paint::Colored),
            "3d",
            "maturing is uncolored"
        );
    }

    /// The two-line headline tag brackets the coarse age and carries the band
    /// color across the whole `[age]` unit; plain paint is the bare `[age]`.
    #[test]
    fn tag_brackets_the_age_and_colors_by_band() {
        console::set_colors_enabled(true);
        // 1 day ago against a fixed clock → caution band (bold yellow).
        let now = SystemTime::UNIX_EPOCH + days(1_000);
        let scale = AgeScale::at(now, T);
        let badge = scale
            .badge(UnixTime::new(999 * 86_400), false)
            .expect("known past");
        assert_eq!(badge.band(), FreshnessBand::Caution);

        assert_eq!(badge.tag(Paint::Plain), "[1d]");
        let colored = badge.tag(Paint::Colored);
        assert!(
            colored.contains('\u{1b}'),
            "caution tag is colored: {colored:?}"
        );
        assert_eq!(console::strip_ansi_codes(&colored), "[1d]");
    }
}
