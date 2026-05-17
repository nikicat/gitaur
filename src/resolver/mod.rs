//! Recursive dependency resolution: targets → ordered Plan.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::index::secondary::{self, Secondary};
use crate::index::IndexFile;
use crate::pacman::alpm_db::PacmanIndex;
use std::collections::{BTreeSet, HashMap, HashSet};
use tracing::{debug, info, instrument};

pub mod classify;
pub mod topo;

pub use classify::{classify, Source};

/// Resolved install plan partitioned by source.
#[derive(Debug, Default, Clone)]
pub struct Plan {
    /// Direct targets the user named that resolve to a sync repo. Installed
    /// without `--asdeps` so pacman records them as explicit.
    pub direct_repo: Vec<String>,
    /// Transitive repo pkgnames pulled in via AUR builds; installed with `--asdeps`.
    pub transitive_repo: Vec<String>,
    /// AUR pkgbases in build order.
    pub aur_order: Vec<String>,
    /// User-requested top-level targets (pkgnames, not pkgbases).
    pub direct_targets: HashSet<String>,
}

/// Resolve `targets` against the index + pacman DBs into a [`Plan`].
///
/// `by` is `None` when no AUR index is loaded (typical fresh installs where
/// the user hasn't run `-Sy` yet); classification then degenerates to
/// pacman-only and any unknown name short-circuits to [`Source::Missing`].
#[instrument(skip(_cfg, idx, by, pac), fields(targets = targets.len()))]
pub fn resolve(
    _cfg: &Config,
    idx: &IndexFile,
    by: Option<&Secondary>,
    pac: &PacmanIndex,
    targets: &[String],
) -> Result<Plan> {
    let mut plan = Plan::default();
    let mut visited_aur: BTreeSet<String> = BTreeSet::new();
    let mut missing: Vec<String> = Vec::new();
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();

    let direct_set: HashSet<String> = targets
        .iter()
        .map(|t| secondary::strip_version_constraint(t).to_string())
        .collect();
    for t in targets {
        plan.direct_targets.insert(t.clone());
    }

    // BFS: each queued entry carries `is_direct` so a top-level repo target
    // lands in `direct_repo` (explicit), while a repo dep pulled by an AUR
    // build lands in `transitive_repo` (--asdeps).
    let mut queue: Vec<(String, bool)> = targets.iter().map(|t| (t.clone(), true)).collect();
    while let Some((target, is_direct)) = queue.pop() {
        let bare = secondary::strip_version_constraint(&target).to_string();
        match classify(by, pac, &bare) {
            Source::Installed => {
                debug!(target = %bare, "already installed");
            }
            Source::Repo => {
                let direct = is_direct || direct_set.contains(&bare);
                let bucket = if direct {
                    &mut plan.direct_repo
                } else {
                    &mut plan.transitive_repo
                };
                if !bucket.iter().any(|s| s == &bare) {
                    bucket.push(bare);
                }
            }
            Source::Aur(entry_idx) => {
                let entry = &idx.entries[entry_idx];
                let pkgbase = entry.pkgbase.clone();
                if !visited_aur.insert(pkgbase.clone()) {
                    continue;
                }
                let deps: Vec<String> = entry
                    .depends
                    .iter()
                    .chain(entry.makedepends.iter())
                    .chain(entry.checkdepends.iter())
                    .map(|d| secondary::strip_version_constraint(d).to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                edges.insert(pkgbase.clone(), deps.clone());
                queue.extend(deps.into_iter().map(|d| (d, false)));
            }
            Source::Missing => missing.push(bare),
        }
    }

    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        return Err(Error::UnknownTargets(missing.join(", ")));
    }

    plan.aur_order = topo::sort(&edges, &visited_aur)?;
    info!(
        direct_repo = plan.direct_repo.len(),
        transitive_repo = plan.transitive_repo.len(),
        aur = plan.aur_order.len(),
        "plan resolved",
    );
    Ok(plan)
}
