//! Upgrade-procedure support for the shell.
//!
//! The cart replaces the old `upgrade_loop` driver (refresh → picker → apply
//! loop), but the loop's *reusable* machinery lives on here: the refresh+reload
//! step, the dep-row extraction the unified `show` table renders, and the
//! build-time cost overlay. The shell's `show` feeds these into
//! [`ui::transaction_table`]; `apply` feeds them into [`ui::cost_summary`].

use super::cart::{Cart, Source};
use crate::build;
use crate::build::UpgradeSession;
use crate::build::metrics::MetricsStore;
use crate::config::Config;
use crate::error::Result;
use crate::index::IndexEntry;
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

/// Refresh the mirror + index (subject to `policy`), then reload the session so
/// subsequent `search`/`info`/`upgrade` see current data. Under
/// [`FetchPolicy::WhenStale`] a recently-fetched mirror skips the network fetch,
/// but the in-memory session is still reloaded from disk so an external `pacman
/// -Sy`/`-Syu` is reflected. `Ok(None)` when no index exists even after
/// (shouldn't happen once a clone is on disk, but the caller degrades
/// gracefully).
pub(crate) fn refresh_and_reload(
    cfg: &Config,
    policy: FetchPolicy,
) -> Result<Option<UpgradeSession>> {
    if should_fetch(cfg, policy) {
        mirror::cmd_refresh(cfg, false)?;
    } else {
        debug!("mirror fetched within the refresh TTL; reloading from disk without a fetch");
    }
    UpgradeSession::load(cfg)
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
    session: &UpgradeSession,
    cart: &Cart,
) -> Vec<PreflightNote> {
    issues
        .into_iter()
        .map(|issue| {
            let remedy = match &issue {
                preflight::Issue::UnsatisfiedDep { target, depend, .. } => {
                    let entry = session.secondary().lookup(session.index(), target.as_str());
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

/// Print one preflight note: the pacman-parity issue line plus the remediation
/// hint. Resolved-by-staged-rebuild renders as an informational note, not a
/// warning — the transaction as staged already handles it.
pub(crate) fn print_preflight_note(note: &PreflightNote) {
    match &note.remedy {
        Remedy::StagedRebuild { target } => {
            ui::note(&format!(
                "{} — resolved by the staged rebuild of {target} (built and installed before the repo upgrade)",
                note.issue
            ));
        }
        Remedy::AddRebuild { target } => {
            ui::warn(&note.issue.to_string());
            ui::note(&format!(
                "`add {target}` stages a rebuild — its AUR package no longer needs the broken dependency"
            ));
            if let Some(alt) = pin_hint(&note.issue) {
                ui::note(&alt);
            }
        }
        Remedy::RebuildWontHelp => {
            ui::warn(&note.issue.to_string());
            if let preflight::Issue::UnsatisfiedDep { target, depend, .. } = &note.issue {
                ui::note(&format!(
                    "the AUR {target} still depends on '{depend}', so a rebuild won't help"
                ));
            }
            if let Some(alt) = pin_hint(&note.issue) {
                ui::note(&alt);
            }
        }
        Remedy::Unknown => {
            ui::warn(&note.issue.to_string());
            if let Some(alt) = pin_hint(&note.issue) {
                ui::note(&alt);
            }
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
    session: &UpgradeSession,
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
        let Some(pb) = session.pkgbase_of(&r.name) else {
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
            if pkgbase_built(session, pb) {
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
fn pkgbase_built(session: &UpgradeSession, pb: &PkgBase) -> bool {
    session
        .secondary()
        .lookup_pkgbase(session.index(), pb)
        .is_some_and(|entry| {
            let pkgnames: Vec<PkgName> = entry.pkgnames.iter().map(|p| p.name.clone()).collect();
            build::artifacts_built(pb, &entry.version(), &pkgnames)
        })
}

#[cfg(test)]
mod tests {
    use super::{FetchPolicy, Remedy, rebuild_remedy, should_fetch};
    use crate::config::Config;
    use crate::index::IndexEntry;
    use crate::names::{PkgName, PkgTarget};
    use crate::paths;
    use crate::testing::ScopedStateRoot;
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
}
