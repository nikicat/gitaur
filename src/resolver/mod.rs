//! Recursive dependency resolution: targets → ordered Plan.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::index::secondary::{self, Secondary};
use crate::index::IndexFile;
use alpm::Alpm;
use std::collections::{BTreeSet, HashMap, HashSet};
use tracing::{debug, info, instrument};

pub mod classify;
pub mod topo;

pub use classify::{classify, Source};

/// Resolved install plan partitioned by source.
#[derive(Debug, Default, Clone)]
pub struct Plan {
    /// Repo pkgnames to install via one batched pacman `-S` call.
    pub repo_deps: Vec<String>,
    /// AUR pkgbases in build order.
    pub aur_order: Vec<String>,
    /// User-requested top-level targets (pkgnames, not pkgbases).
    pub direct_targets: HashSet<String>,
}

/// Resolve `targets` against the index + alpm DBs into a [`Plan`].
#[instrument(skip(_cfg, idx, by, alpm), fields(targets = targets.len()))]
pub fn resolve(
    _cfg: &Config,
    idx: &IndexFile,
    by: &Secondary,
    alpm: &Alpm,
    targets: &[String],
) -> Result<Plan> {
    let mut plan = Plan::default();
    let mut visited_aur: BTreeSet<String> = BTreeSet::new();
    let mut missing: Vec<String> = Vec::new();
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    let mut queue: Vec<String> = targets.to_vec();

    for t in targets {
        plan.direct_targets.insert(t.clone());
    }

    while let Some(target) = queue.pop() {
        let bare = secondary::strip_version_constraint(&target).to_string();
        match classify(idx, by, alpm, &bare) {
            Source::Installed => {
                debug!(target = %bare, "already installed");
            }
            Source::Repo => {
                if !plan.repo_deps.iter().any(|s| s == &bare) {
                    plan.repo_deps.push(bare);
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
                queue.extend(deps);
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
        repo = plan.repo_deps.len(),
        aur = plan.aur_order.len(),
        "plan resolved",
    );
    Ok(plan)
}
