//! Upgrade-procedure support for the shell.
//!
//! The cart replaces the old `upgrade_loop` driver (refresh → picker → apply
//! loop), but the loop's *reusable* machinery lives on here: the refresh+reload
//! step, the dep-row extraction the unified `show` table renders, and the
//! build-time cost overlay. The shell's `show` feeds these into
//! [`ui::transaction_table`]; `apply` feeds them into [`ui::cost_summary`].

use super::cart::{Cart, Source};
use crate::build;
use crate::build::metrics::MetricsStore;
use crate::config::Config;
use crate::error::Result;
use crate::index::{AurIndexData, IndexEntry};
use crate::mirror;
use crate::names::{PkgBase, PkgName, PkgTarget, RepoRank};
use crate::pacman::alpm_db::{self, PacmanIndex};
use crate::pacman::{preflight, sync};
use crate::paths;
use crate::resolver::Plan;
use crate::ui;
use crate::ui::{PreviewMetrics, TxnRoot};
use indicatif::MultiProgress;
use std::collections::HashSet;
use std::time::{Duration, SystemTime};
use tracing::{debug, warn};

/// Whether a session reload re-fetches the mirror unconditionally or only when
/// the on-disk clone has gone stale.
///
/// A named policy rather than a bare bool so the call sites read intent: the
/// explicit `refresh` command forces a fetch, while `upgrade` defers to the TTL
/// so back-to-back `upgrade`s don't each re-hit the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchPolicy {
    /// Always fetch — the explicit `refresh` command.
    Always,
    /// Fetch only when the last fetch predates
    /// [`Config::refresh_max_age_secs`]; otherwise reload from disk without a
    /// network round-trip — `upgrade`'s default.
    WhenStale,
}

/// One reload's result: what the refresh did to the AUR mirror (when one
/// ran at all) plus the freshly loaded AUR data.
pub(crate) struct AurReload {
    /// `None` when the TTL said the mirror was fresh and no fetch ran.
    pub outcome: Option<mirror::RefreshOutcome>,
    /// The reloaded AUR data — *empty* when there's still no index on disk
    /// (bootstrap declined, AUR disabled); see [`AurIndexData::load`].
    pub data: AurIndexData,
}

/// Refresh the mirror + index (subject to `policy`), then reload the session so
/// subsequent `search`/`info`/`upgrade` see current data. Under
/// [`FetchPolicy::WhenStale`] a recently-fetched mirror skips the network fetch,
/// but the in-memory AUR data is still reloaded from disk so an external `pacman
/// -Sy`/`-Syu` is reflected. The first-ever fetch needs the bootstrap clone,
/// which [`mirror::cmd_refresh`]'s consent gate announces and confirms; a
/// decline surfaces in [`AurReload::outcome`] so the caller can hint.
pub(crate) fn refresh_and_reload(cfg: &Config, policy: FetchPolicy) -> Result<AurReload> {
    let outcome = if should_fetch(cfg, policy) {
        Some(mirror::cmd_refresh(cfg, mirror::RefreshReason::Shell)?)
    } else {
        debug!("mirror fetched within the refresh TTL; reloading from disk without a fetch");
        None
    };
    Ok(AurReload {
        outcome,
        data: AurIndexData::load(cfg)?,
    })
}

/// Whether [`refresh_and_reload`] should hit the network: always under
/// [`FetchPolicy::Always`], else only once the mirror is older than
/// [`Config::refresh_max_age_secs`] (or was never fetched).
fn should_fetch(cfg: &Config, policy: FetchPolicy) -> bool {
    match policy {
        FetchPolicy::Always => true,
        FetchPolicy::WhenStale => match mirror::last_fetch_age() {
            Some(age) => age >= Duration::from_secs(cfg.refresh_max_age_secs),
            None => true,
        },
    }
}

/// How fresh the rootless sync db must be for an apply-time preflight to skip
/// its own re-sync. Deliberately tight — just wide enough to dedupe the
/// common `upgrade` → immediate `apply` flow, where the session synced
/// seconds ago. Anything longer re-syncs: the guard exists to close the gap
/// between the last sync and what `pacman -Syu`'s own `-Sy` is about to
/// fetch, and that gap grows with every minute spent reviewing the cart.
const APPLY_RESYNC_MAX_AGE: Duration = Duration::from_mins(1);

/// Bring the rootless sync db up to date right before an apply's repo lane, so
/// the sysupgrade preflight (and the recomputed `--ignore` set) sees close to
/// the same data the imminent `pacman -Syu` will fetch — the drift guard.
///
/// Skipped when [`Config::check_repo_updates`] is off (there is no rootless
/// store to sync) or when the last refresh is younger than
/// [`APPLY_RESYNC_MAX_AGE`]. Failures — offline, Ctrl+C on the refresh lock —
/// degrade to a warning: the preflight then runs on the staler snapshot, which
/// is still advisory.
pub(crate) fn resync_repo_dbs(cfg: &Config) {
    if !cfg.check_repo_updates {
        return;
    }
    if mirror::last_fetch_age().is_some_and(|age| age < APPLY_RESYNC_MAX_AGE) {
        debug!("repo dbs synced recently; skipping the apply-time re-sync");
        return;
    }
    if let Err(e) = sync::refresh_sync_db(&MultiProgress::new()) {
        ui::warn(&format!(
            "repo db re-sync before the upgrade failed: {e} — checking against the last-synced data"
        ));
    }
}

/// A sysupgrade preflight issue paired with the shell-native way out —
/// computed against the AUR index and the staged cart, rendered under the
/// `show` preview and ahead of `apply`'s confirm/sudo gates.
pub(crate) struct PreflightNote {
    pub(crate) issue: preflight::Issue,
    pub(crate) remedy: Remedy,
}

/// The AUR-aware remediation for one sysupgrade preflight issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Remedy {
    /// A staged AUR rebuild of `target` drops the broken dependency —
    /// installing it ahead of the repo lane resolves the issue, so `apply`
    /// orders it first instead of blocking.
    StagedRebuild { target: PkgName },
    /// Rebuilding `target` from its current AUR PKGBUILD would fix it (the
    /// PKGBUILD's deps have moved on) — suggest `add <target>`.
    AddRebuild { target: PkgName },
    /// The AUR PKGBUILD still declares the broken dependency; a rebuild
    /// wouldn't help. Pin or uninstall are what's left.
    RebuildWontHelp,
    /// No AUR-side fix known (not an AUR package, a conflict, …).
    Unknown,
}

/// Pair each preflight issue with its remedy, resolved against the AUR index
/// (does the current PKGBUILD still need the broken dep?) and the cart (is the
/// fixing rebuild already staged?).
pub(crate) fn preflight_notes(
    issues: Vec<preflight::Issue>,
    aur_data: &AurIndexData,
    cart: &Cart,
) -> Vec<PreflightNote> {
    issues
        .into_iter()
        .map(|issue| {
            let remedy = match &issue {
                preflight::Issue::UnsatisfiedDep { target, depend, .. } => {
                    let entry = aur_data.entry(target.as_str());
                    let staged = cart
                        .item(&PkgTarget::from(target))
                        .is_some_and(|it| it.source == Source::Aur);
                    rebuild_remedy(entry, target, depend, staged)
                }
                _ => Remedy::Unknown,
            };
            PreflightNote { issue, remedy }
        })
        .collect()
}

/// Whether rebuilding `target` from its current AUR PKGBUILD would satisfy the
/// preflight: the entry's runtime `depends` no longer reference the broken
/// dep-spec's name. Pure over index data so the verdict table is unit-testable.
///
/// `entry.depends` is the pkgbase-flat union (per-pkgname sections fold in —
/// see `index::srcinfo`), so a split sibling still needing the dep
/// conservatively reads as "won't help".
fn rebuild_remedy(
    entry: Option<&IndexEntry>,
    target: &PkgName,
    broken: &PkgTarget,
    staged: bool,
) -> Remedy {
    let Some(entry) = entry else {
        return Remedy::Unknown;
    };
    let still_needs = entry.depends.iter().any(|d| d.bare() == broken.bare());
    if still_needs {
        Remedy::RebuildWontHelp
    } else if staged {
        Remedy::StagedRebuild {
            target: target.clone(),
        }
    } else {
        Remedy::AddRebuild {
            target: target.clone(),
        }
    }
}

/// One rendered line of a preflight note, tagged with the ui channel it goes
/// through — pure data, so the user-facing wording is unit-testable.
#[derive(Debug, PartialEq, Eq)]
enum NoteLine {
    Warn(String),
    Note(String),
}

impl PreflightNote {
    /// The lines this note renders as: the pacman-parity issue line (a
    /// warning — or an informational note when the staged cart already
    /// resolves it) followed by the remediation hints.
    fn lines(&self) -> Vec<NoteLine> {
        match &self.remedy {
            Remedy::StagedRebuild { target } => vec![NoteLine::Note(format!(
                "{} — resolved by the staged rebuild of {target} (built and installed before the repo upgrade)",
                self.issue
            ))],
            Remedy::AddRebuild { target } => {
                let mut lines = vec![
                    NoteLine::Warn(self.issue.to_string()),
                    NoteLine::Note(format!(
                        "`add {target}` stages a rebuild — its AUR package no longer needs the broken dependency"
                    )),
                ];
                lines.extend(pin_hint(&self.issue).map(NoteLine::Note));
                lines
            }
            Remedy::RebuildWontHelp => {
                let mut lines = vec![NoteLine::Warn(self.issue.to_string())];
                if let preflight::Issue::UnsatisfiedDep { target, depend, .. } = &self.issue {
                    lines.push(NoteLine::Note(format!(
                        "the AUR {target} still depends on '{depend}', so a rebuild won't help"
                    )));
                }
                lines.extend(pin_hint(&self.issue).map(NoteLine::Note));
                lines
            }
            Remedy::Unknown => {
                let mut lines = vec![NoteLine::Warn(self.issue.to_string())];
                lines.extend(pin_hint(&self.issue).map(NoteLine::Note));
                lines
            }
        }
    }
}

/// Print one preflight note: the pacman-parity issue line plus the remediation
/// hint. Resolved-by-staged-rebuild renders as an informational note, not a
/// warning — the transaction as staged already handles it.
pub(crate) fn print_preflight_note(note: &PreflightNote) {
    for line in note.lines() {
        match line {
            NoteLine::Warn(msg) => ui::warn(&msg),
            NoteLine::Note(msg) => ui::note(&msg),
        }
    }
}

/// The pin/uninstall escape hatches for a broken-dep issue: dropping the
/// breaking package's upgrade row keeps today's state (`--ignore`, a partial
/// upgrade); removing the dependent clears the constraint entirely.
fn pin_hint(issue: &preflight::Issue) -> Option<String> {
    let preflight::Issue::UnsatisfiedDep {
        target, causing, ..
    } = issue
    else {
        return None;
    };
    Some(match causing {
        Some(c) => format!(
            "`drop {c}` pins it for now (partial upgrade), or `remove {target}` uninstalls the dependent"
        ),
        None => format!("`remove {target}` uninstalls the dependent"),
    })
}

/// A system-dbpath pacman snapshot — `pacman -S/-U/-Syu` act against this db, so
/// the apply plan must match what pacman will see.
pub(crate) fn system_pac() -> Result<PacmanIndex> {
    let alpm = alpm_db::open()?;
    Ok(PacmanIndex::build(&alpm))
}

/// A rootless-synced snapshot, used only for the preview's *size* figures: the
/// candidate versions came from this freshly-synced db, so its syncdb carries
/// the new versions whose archives aren't cached — `download_size()` then
/// reports the real fetch cost rather than the `0 B` the stale system syncdb
/// yields for an already-cached installed version.
pub(crate) fn synced_pac() -> Result<PacmanIndex> {
    let alpm = alpm_db::open_synced()?;
    Ok(PacmanIndex::build(&alpm))
}

/// How old a recorded build duration may be before the preview dims it. Build
/// flows drift as compilers/libs/ccache move on, so a months-old measurement is
/// a shaky predictor; 90 days balances "covers routine rebuilds" against
/// "anything older deserves the dim".
const STALE_METRIC_AGE_SECS: u64 = 90 * 24 * 3_600;

/// The dep rows the unified table shows beneath the roots: concrete repo
/// pkgnames the build pulls in, plus AUR pkgbases that were dragged in rather
/// than named.
pub(crate) fn dep_rows(plan: Option<&Plan>) -> (Vec<PkgName>, Vec<PkgBase>) {
    let Some(plan) = plan else {
        return (Vec::new(), Vec::new());
    };
    let repo_deps = plan.transitive_repo.iter().map(PkgName::from).collect();
    let root_bases: HashSet<&PkgBase> = plan.direct_aur.iter().collect();
    let aur_deps = plan
        .aur_strata
        .iter()
        .flatten()
        .filter(|pb| !root_bases.contains(pb))
        .cloned()
        .collect();
    (repo_deps, aur_deps)
}

/// Build the per-row build-time + `built` overlay for the preview from the
/// cross-session metrics store. Empty overlay on any store failure (the preview
/// then shows `~?` for AUR rows, which is correct and never blocks apply).
pub(crate) fn preview_metrics(
    aur_data: &AurIndexData,
    roots: &[TxnRoot],
    plan: Option<&Plan>,
) -> PreviewMetrics {
    let store = MetricsStore::open(&paths::metrics_db_path())
        .inspect_err(
            |e| warn!(error = %e, "open metrics store for preview; skipping build-time column"),
        )
        .ok();
    let mut out = PreviewMetrics::empty();
    let now = SystemTime::now();

    // AUR roots only — repo upgrades have no build, so no build-time/built cell.
    for r in roots.iter().filter(|r| r.repo.rank() == RepoRank::Aur) {
        let Some(pb) = aur_data.pkgbase_of(&r.name) else {
            continue;
        };
        if row_built(pb, r) {
            out.built_roots.insert(r.name.clone());
        }
        if let Some(store) = &store {
            insert_root_time(&mut out, store, &r.name, pb, now);
        }
    }

    // Pulled-in AUR deps: pkgbases in the strata that weren't named roots.
    if let Some(plan) = plan {
        let root_bases: HashSet<&PkgBase> = plan.direct_aur.iter().collect();
        let dep_pkgbases: Vec<&PkgBase> = plan
            .aur_strata
            .iter()
            .flatten()
            .filter(|pb| !root_bases.contains(pb))
            .collect();
        for pb in &dep_pkgbases {
            if pkgbase_built(aur_data, pb) {
                out.built_deps.insert((*pb).clone());
            }
        }
        if let Some(store) = &store {
            match store.latest_build_many(dep_pkgbases.iter().copied()) {
                Ok(map) => {
                    out.dep_build_secs = map
                        .into_iter()
                        .map(|(pb, rec)| (pb, rec.build_secs))
                        .collect();
                }
                Err(e) => warn!(error = %e, "bulk lookup AUR dep build times"),
            }
        }
    }
    out
}

/// Whether a single AUR root row's artifact is already built on disk
/// (per-pkgname, like the picker's `built` tag — see [`build::artifacts_built`]).
/// A row with no resolved `new_ver` (couldn't look one up) counts as not built.
fn row_built(pb: &PkgBase, r: &TxnRoot) -> bool {
    r.new_ver
        .as_ref()
        .is_some_and(|v| build::artifacts_built(pb, v, std::slice::from_ref(&r.name)))
}

/// Record `pkgbase`'s latest build duration into the root overlay keyed by
/// `name`, flagging it stale past [`STALE_METRIC_AGE_SECS`]. A lookup error
/// warns and leaves the row at `~?`; a clock-skew age error under-dims rather
/// than wrongly disparaging a fresh figure.
fn insert_root_time(
    out: &mut PreviewMetrics,
    store: &MetricsStore,
    name: &PkgName,
    pb: &PkgBase,
    now: SystemTime,
) {
    match store.latest_build(pb) {
        Ok(Some(rec)) => {
            out.root_build_secs.insert(name.clone(), rec.build_secs);
            if rec.age(now).is_ok_and(|a| a >= STALE_METRIC_AGE_SECS) {
                out.stale.insert(name.clone());
            }
        }
        Ok(None) => {}
        Err(e) => warn!(error = %e, pkgbase = %pb, "lookup AUR root build time"),
    }
}

/// Whether `pkgbase`'s build is complete on disk — every pkgname it produces at
/// the index version has an artifact. Used for the pulled-in AUR **dep** rows
/// (labelled by pkgbase, unlike per-pkgname roots). `false` when the pkgbase
/// isn't in the index.
fn pkgbase_built(aur_data: &AurIndexData, pb: &PkgBase) -> bool {
    aur_data
        .lookup()
        .lookup_pkgbase(aur_data.index(), pb)
        .is_some_and(|entry| {
            let pkgnames: Vec<PkgName> = entry.pkgnames.iter().map(|p| p.name.clone()).collect();
            build::artifacts_built(pb, &entry.version(), &pkgnames)
        })
}

#[cfg(test)]
mod tests {
    use super::{
        FetchPolicy, NoteLine, PreflightNote, Remedy, pin_hint, rebuild_remedy, resync_repo_dbs,
        should_fetch,
    };
    use crate::config::Config;
    use crate::index::IndexEntry;
    use crate::names::{PkgName, PkgTarget};
    use crate::pacman::preflight;
    use crate::paths;
    use crate::testing::ScopedStateRoot;
    use crate::{assert_contains, assert_not_contains, assert_regex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tempfile::TempDir;

    /// An AUR index entry whose runtime `depends` are exactly `deps`.
    fn entry_with_deps(deps: &[&str]) -> IndexEntry {
        IndexEntry {
            pkgbase: "ioquake3-git".into(),
            depends: deps.iter().map(|d| PkgTarget::new(*d)).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn rebuild_remedy_classifies_the_libjpeg_shapes() {
        let target = PkgName::new("ioquake3-git");
        let broken = PkgTarget::new("libjpeg");

        // The AUR PKGBUILD moved on (depends on libjpeg-turbo now): a rebuild
        // fixes it — staged reads as a blocker, unstaged as an `add` hint.
        let moved_on = entry_with_deps(&["libjpeg-turbo", "sdl2"]);
        assert_eq!(
            rebuild_remedy(Some(&moved_on), &target, &broken, false),
            Remedy::AddRebuild {
                target: target.clone()
            }
        );
        assert_eq!(
            rebuild_remedy(Some(&moved_on), &target, &broken, true),
            Remedy::StagedRebuild {
                target: target.clone()
            }
        );

        // The PKGBUILD still declares the broken dep (with or without a
        // version constraint): rebuilding won't help.
        let still_needs = entry_with_deps(&["libjpeg", "sdl2"]);
        assert_eq!(
            rebuild_remedy(Some(&still_needs), &target, &broken, true),
            Remedy::RebuildWontHelp
        );
        let versioned = entry_with_deps(&["libjpeg>=8"]);
        assert_eq!(
            rebuild_remedy(Some(&versioned), &target, &broken, false),
            Remedy::RebuildWontHelp
        );

        // Not in the AUR index at all: no AUR-side fix to suggest.
        assert_eq!(
            rebuild_remedy(None, &target, &broken, false),
            Remedy::Unknown
        );
    }

    #[test]
    fn rebuild_remedy_matches_on_the_bare_dep_name() {
        // The preflight reports the *dep spec* (`libbar>=2`); the PKGBUILD may
        // carry its own constraint. The verdict must compare bare names, not
        // spec strings.
        let target = PkgName::new("foo");
        let broken = PkgTarget::new("libbar>=2");
        let still_needs = entry_with_deps(&["libbar=1.5"]);
        assert_eq!(
            rebuild_remedy(Some(&still_needs), &target, &broken, false),
            Remedy::RebuildWontHelp
        );
        let moved_on = entry_with_deps(&["libbar-ng"]);
        assert_eq!(
            rebuild_remedy(Some(&moved_on), &target, &broken, false),
            Remedy::AddRebuild { target }
        );
    }

    /// Write the AUR fetch stamp `ago` seconds in the past (the format
    /// `mirror::record_fetch_stamp` emits — a Unix-epoch seconds string).
    fn stamp_secs_ago(ago: u64) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        std::fs::write(paths::fetch_stamp_path(), (now - ago).to_string()).unwrap();
    }

    #[test]
    fn when_stale_fetches_only_past_the_ttl() {
        let dir = TempDir::new().unwrap();
        let _root = ScopedStateRoot::new(dir.path().to_path_buf());
        let cfg = Config::default();
        let ttl = cfg.refresh_max_age_secs;

        // No stamp yet → never fetched → fetch.
        assert!(should_fetch(&cfg, FetchPolicy::WhenStale));

        // Stamped within the TTL → skip the network fetch.
        stamp_secs_ago(ttl / 2);
        assert!(!should_fetch(&cfg, FetchPolicy::WhenStale));

        // Stamped past the TTL → stale → fetch.
        stamp_secs_ago(ttl * 2);
        assert!(should_fetch(&cfg, FetchPolicy::WhenStale));
    }

    #[test]
    fn always_policy_ignores_a_fresh_stamp() {
        let dir = TempDir::new().unwrap();
        let _root = ScopedStateRoot::new(dir.path().to_path_buf());
        let cfg = Config::default();
        stamp_secs_ago(0); // fresh — would skip under WhenStale
        assert!(should_fetch(&cfg, FetchPolicy::Always));
    }

    #[test]
    fn a_garbled_stamp_reads_as_stale() {
        let dir = TempDir::new().unwrap();
        let _root = ScopedStateRoot::new(dir.path().to_path_buf());
        let cfg = Config::default();
        std::fs::write(paths::fetch_stamp_path(), "not-a-number").unwrap();
        assert!(should_fetch(&cfg, FetchPolicy::WhenStale));
    }

    #[test]
    fn apply_resync_skips_without_touching_the_sync_db() {
        let dir = TempDir::new().unwrap();
        let _root = ScopedStateRoot::new(dir.path().to_path_buf());

        // Repo-update checking off: there is no rootless store to sync.
        let cfg = Config {
            check_repo_updates: false,
            ..Config::default()
        };
        resync_repo_dbs(&cfg);
        assert!(
            !paths::sync_db_path().exists(),
            "no sync db may be created when check_repo_updates is off"
        );

        // Checking on, but synced seconds ago: the drift guard dedupes the
        // common `upgrade` → immediate `apply` flow.
        let cfg = Config {
            check_repo_updates: true,
            ..Config::default()
        };
        stamp_secs_ago(0);
        resync_repo_dbs(&cfg);
        assert!(
            !paths::sync_db_path().exists(),
            "a fresh stamp must skip the re-sync entirely"
        );
    }

    /// A broken-dep preflight issue, with or without the `causing` package
    /// (pacman's "installing X breaks…" vs plain "can't satisfy…" shapes).
    fn broken_dep(causing: Option<&str>) -> preflight::Issue {
        preflight::Issue::UnsatisfiedDep {
            target: PkgName::new("ioquake3-git"),
            depend: PkgTarget::new("libjpeg"),
            causing: causing.map(PkgName::new),
            causing_ver: None,
        }
    }

    fn conflict() -> preflight::Issue {
        preflight::Issue::Conflict {
            pkg1: PkgName::new("foo"),
            pkg2: PkgName::new("bar"),
            reason: PkgTarget::new("foo"),
        }
    }

    #[test]
    fn pin_hint_offers_drop_only_when_a_causing_package_exists() {
        let hint = pin_hint(&broken_dep(Some("libjpeg-turbo"))).expect("broken dep hints");
        assert_regex!(
            hint,
            "^`drop libjpeg-turbo` pins .* `remove ioquake3-git` uninstalls"
        );
        let hint = pin_hint(&broken_dep(None)).expect("broken dep hints");
        assert_regex!(hint, "^`remove ioquake3-git` uninstalls");
        assert_not_contains!(hint, "`drop");
        // Only broken deps have a pin escape hatch; a conflict offers none.
        assert_eq!(pin_hint(&conflict()), None);
    }

    #[test]
    fn staged_rebuild_renders_as_a_single_resolved_note() {
        let note = PreflightNote {
            issue: broken_dep(Some("libjpeg-turbo")),
            remedy: Remedy::StagedRebuild {
                target: PkgName::new("ioquake3-git"),
            },
        };
        let lines = note.lines();
        let [NoteLine::Note(line)] = lines.as_slice() else {
            panic!("staged-rebuild must render as exactly one note: {lines:?}");
        };
        assert_regex!(
            line,
            "breaks dependency 'libjpeg' required by ioquake3-git — resolved by \
             the staged rebuild of ioquake3-git"
        );
    }

    #[test]
    fn add_rebuild_warns_then_hints_add_and_pin() {
        let note = PreflightNote {
            issue: broken_dep(Some("libjpeg-turbo")),
            remedy: Remedy::AddRebuild {
                target: PkgName::new("ioquake3-git"),
            },
        };
        let lines = note.lines();
        assert_eq!(lines.len(), 3, "warn + add hint + pin hint: {lines:?}");
        let NoteLine::Warn(issue) = &lines[0] else {
            panic!("the issue line must be a warning: {lines:?}");
        };
        assert_contains!(issue, "breaks dependency 'libjpeg'");
        let NoteLine::Note(add) = &lines[1] else {
            panic!("the add hint must be a note: {lines:?}");
        };
        assert_regex!(add, "^`add ioquake3-git` stages a rebuild");
        let NoteLine::Note(pin) = &lines[2] else {
            panic!("the pin hint must be a note: {lines:?}");
        };
        assert_contains!(pin, "`drop libjpeg-turbo`");
    }

    #[test]
    fn rebuild_wont_help_explains_the_still_declared_dep() {
        let note = PreflightNote {
            issue: broken_dep(None),
            remedy: Remedy::RebuildWontHelp,
        };
        let lines = note.lines();
        assert_eq!(
            lines.len(),
            3,
            "warn + won't-help note + pin hint: {lines:?}"
        );
        assert!(matches!(&lines[0], NoteLine::Warn(_)), "{lines:?}");
        let NoteLine::Note(wont_help) = &lines[1] else {
            panic!("the won't-help line must be a note: {lines:?}");
        };
        assert_regex!(
            wont_help,
            "^the AUR ioquake3-git still depends on 'libjpeg', so a rebuild won't help"
        );
    }

    #[test]
    fn unknown_remedy_warns_without_hints_for_a_conflict() {
        let note = PreflightNote {
            issue: conflict(),
            remedy: Remedy::Unknown,
        };
        // A conflict has no pin/uninstall escape hatch, so the pacman-parity
        // warning stands alone.
        assert_eq!(
            note.lines(),
            vec![NoteLine::Warn("foo and bar are in conflict".to_owned())]
        );
    }
}
