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
    /// AUR pkgbases grouped into Kahn strata over the **makedepends +
    /// checkdepends** subgraph only — these are the build-time requirements
    /// that must already be installed in localdb when `makepkg` runs.
    /// Stratum N is built and `pacman -U`'d before stratum N+1 begins.
    /// Plain runtime `depends` don't constrain build order — they only need
    /// to resolve in the eventual install transaction.
    pub aur_strata: Vec<Vec<String>>,
    /// User-requested top-level targets (pkgnames, not pkgbases).
    pub direct_targets: HashSet<String>,
}

impl Plan {
    /// Flatten all AUR pkgbases in build order. Convenience for code paths
    /// that don't care about stratum boundaries (counts, displays, …).
    pub fn aur_order(&self) -> Vec<String> {
        self.aur_strata.iter().flatten().cloned().collect()
    }
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
    // Separate graphs:
    //   * `all_edges` covers runtime + build-time deps; used for cycle
    //     detection so a cycle through plain `depends` is still rejected.
    //   * `make_edges` covers only build-time deps (makedepends +
    //     checkdepends) that resolve to AUR pkgbases; drives strata so
    //     each pkg's build-time AUR deps are installed before it runs.
    let mut all_edges: HashMap<String, Vec<String>> = HashMap::new();
    let mut make_edges: HashMap<String, Vec<String>> = HashMap::new();
    let mut pkgname_to_pkgbase: HashMap<String, String> = HashMap::new();

    let direct_set: HashSet<String> = targets
        .iter()
        .map(|t| secondary::strip_version_constraint(t).to_string())
        .collect();
    for t in targets {
        plan.direct_targets.insert(t.clone());
    }

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
                for name in &entry.pkgnames {
                    pkgname_to_pkgbase.insert(name.clone(), pkgbase.clone());
                }
                if !visited_aur.insert(pkgbase.clone()) {
                    continue;
                }
                let runtime: Vec<String> = entry
                    .depends
                    .iter()
                    .map(|d| secondary::strip_version_constraint(d).to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let build_time: Vec<String> = entry
                    .makedepends
                    .iter()
                    .chain(entry.checkdepends.iter())
                    .map(|d| secondary::strip_version_constraint(d).to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                let all: Vec<String> = runtime.iter().chain(build_time.iter()).cloned().collect();
                all_edges.insert(pkgbase.clone(), all.clone());
                // make_edges entry is finalised once we know which deps
                // resolved to AUR pkgbases (need pkgname → pkgbase map).
                // Stage build-time dep pkgnames; rewrite after the BFS.
                make_edges.insert(pkgbase.clone(), build_time);
                queue.extend(all.into_iter().map(|d| (d, false)));
            }
            Source::Missing => missing.push(bare),
        }
    }

    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        return Err(Error::UnknownTargets(missing.join(", ")));
    }

    // Cycle check over the full dep graph — fails fast if `depends` forms a
    // cycle even when makedepends alone would be acyclic.
    let _ = topo::sort(&all_edges, &visited_aur)?;

    // Rewrite make_edges entries from pkgnames → AUR pkgbases. Drop entries
    // pointing at repo/installed targets (irrelevant for build ordering).
    let make_edges_resolved: HashMap<String, Vec<String>> = make_edges
        .into_iter()
        .map(|(pkgbase, deps)| {
            let resolved: Vec<String> = deps
                .into_iter()
                .filter_map(|d| pkgname_to_pkgbase.get(&d).cloned())
                .filter(|pb| pb != &pkgbase) // self-edges from split pkgs
                .collect();
            (pkgbase, resolved)
        })
        .collect();

    plan.aur_strata = topo::strata(&make_edges_resolved, &visited_aur)?;
    info!(
        direct_repo = plan.direct_repo.len(),
        transitive_repo = plan.transitive_repo.len(),
        aur_strata = plan.aur_strata.len(),
        aur_total = plan.aur_strata.iter().map(Vec::len).sum::<usize>(),
        "plan resolved",
    );
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::defaults::default_config;
    use crate::index::schema::IndexEntry;
    use crate::pacman::alpm_db::PacmanIndex;

    fn entry(pkgbase: &str, depends: &[&str], makedepends: &[&str]) -> IndexEntry {
        entry_full(pkgbase, &[pkgbase], depends, makedepends, &[], &[])
    }

    fn entry_full(
        pkgbase: &str,
        pkgnames: &[&str],
        depends: &[&str],
        makedepends: &[&str],
        checkdepends: &[&str],
        provides: &[&str],
    ) -> IndexEntry {
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: pkgnames.iter().map(|s| (*s).into()).collect(),
            depends: depends.iter().map(|s| (*s).into()).collect(),
            makedepends: makedepends.iter().map(|s| (*s).into()).collect(),
            checkdepends: checkdepends.iter().map(|s| (*s).into()).collect(),
            provides: provides.iter().map(|s| (*s).into()).collect(),
            pkgver: "1".into(),
            pkgrel: "1".into(),
            ..Default::default()
        }
    }

    fn run(targets: &[&str], entries: Vec<IndexEntry>, repo: &[&str]) -> Result<Plan> {
        let idx = IndexFile {
            entries,
            ..IndexFile::empty()
        };
        let by = Secondary::build(&idx);
        let mut pac = PacmanIndex::default();
        for n in repo {
            pac.sync_names.insert((*n).into());
        }
        let cfg = default_config();
        let targets: Vec<String> = targets.iter().map(|s| (*s).to_string()).collect();
        resolve(&cfg, &idx, Some(&by), &pac, &targets)
    }

    // ---- repo-only paths -------------------------------------------------

    #[test]
    fn pure_repo_target_lands_in_direct_repo() {
        let plan = run(&["foo"], vec![], &["foo"]).unwrap();
        assert_eq!(plan.direct_repo, vec!["foo".to_string()]);
        assert!(plan.transitive_repo.is_empty());
        assert!(plan.aur_strata.is_empty());
    }

    #[test]
    fn version_constraint_on_repo_target_strips() {
        let plan = run(&["foo>=1.2"], vec![], &["foo"]).unwrap();
        assert_eq!(plan.direct_repo, vec!["foo".to_string()]);
    }

    #[test]
    fn missing_target_errors() {
        let err = run(&["nope"], vec![], &[]).unwrap_err();
        match err {
            Error::UnknownTargets(s) => assert_eq!(s, "nope"),
            other => panic!("expected UnknownTargets, got {other:?}"),
        }
    }

    // ---- single AUR pkg --------------------------------------------------

    #[test]
    fn aur_with_no_deps_single_stratum() {
        let plan = run(&["a"], vec![entry("a", &[], &[])], &[]).unwrap();
        assert_eq!(plan.aur_strata, vec![vec!["a".to_string()]]);
    }

    #[test]
    fn aur_with_repo_makedep_pulls_transitive_repo() {
        let plan = run(&["a"], vec![entry("a", &[], &["bash"])], &["bash"]).unwrap();
        assert_eq!(plan.aur_strata, vec![vec!["a".to_string()]]);
        assert_eq!(plan.transitive_repo, vec!["bash".to_string()]);
        assert!(plan.direct_repo.is_empty());
    }

    // ---- build graph: makedepends drive strata, depends do not ----------

    #[test]
    fn regular_depends_does_not_create_stratum_edge() {
        // A depends=B (runtime only), both AUR. Both should land in stratum 0.
        let plan = run(
            &["a"],
            vec![entry("a", &["b"], &[]), entry("b", &[], &[])],
            &[],
        )
        .unwrap();
        assert_eq!(plan.aur_strata.len(), 1);
        let mut s0 = plan.aur_strata[0].clone();
        s0.sort();
        assert_eq!(s0, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn makedepends_creates_stratum_edge() {
        // A makedepends=B, both AUR → B must be installed before A builds.
        let plan = run(
            &["a"],
            vec![entry("a", &[], &["b"]), entry("b", &[], &[])],
            &[],
        )
        .unwrap();
        assert_eq!(
            plan.aur_strata,
            vec![vec!["b".to_string()], vec!["a".to_string()]]
        );
    }

    #[test]
    fn deep_makedep_chain_layers_correctly() {
        // a → b → c via makedepends → 3 strata.
        let plan = run(
            &["a"],
            vec![
                entry("a", &[], &["b"]),
                entry("b", &[], &["c"]),
                entry("c", &[], &[]),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(
            plan.aur_strata,
            vec![
                vec!["c".to_string()],
                vec!["b".to_string()],
                vec!["a".to_string()],
            ]
        );
    }

    #[test]
    fn diamond_makedep_graph_groups_middle_layer() {
        // d makedepends b+c; b makedepends a; c makedepends a. Layers: [a],[b,c],[d].
        let plan = run(
            &["d"],
            vec![
                entry("d", &[], &["b", "c"]),
                entry("b", &[], &["a"]),
                entry("c", &[], &["a"]),
                entry("a", &[], &[]),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(plan.aur_strata.len(), 3);
        assert_eq!(plan.aur_strata[0], vec!["a".to_string()]);
        let mut mid = plan.aur_strata[1].clone();
        mid.sort();
        assert_eq!(mid, vec!["b".to_string(), "c".to_string()]);
        assert_eq!(plan.aur_strata[2], vec!["d".to_string()]);
    }

    #[test]
    fn checkdepends_count_as_buildtime() {
        // checkdeps run check() at build time, so they're build-edges too.
        let plan = run(
            &["a"],
            vec![
                entry_full("a", &["a"], &[], &[], &["b"], &[]),
                entry("b", &[], &[]),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(
            plan.aur_strata,
            vec![vec!["b".to_string()], vec!["a".to_string()]]
        );
    }

    #[test]
    fn split_pkg_makedep_collapses_to_one_pkgbase() {
        // Pkg `client` makedepends on `helper-libs`, which is a pkgname of
        // pkgbase `helper-pkg` (split pkg with names [helper, helper-libs]).
        // Strata edge must be on the pkgbase, not the pkgname.
        let plan = run(
            &["client"],
            vec![
                entry_full("client", &["client"], &[], &["helper-libs"], &[], &[]),
                entry_full("helper-pkg", &["helper", "helper-libs"], &[], &[], &[], &[]),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(
            plan.aur_strata,
            vec![vec!["helper-pkg".to_string()], vec!["client".to_string()]]
        );
    }

    #[test]
    fn split_pkg_visited_once() {
        // Two of the split pkgnames named as targets should still result in
        // a single pkgbase visit.
        let plan = run(
            &["helper", "helper-libs"],
            vec![entry_full(
                "helper-pkg",
                &["helper", "helper-libs"],
                &[],
                &[],
                &[],
                &[],
            )],
            &[],
        )
        .unwrap();
        assert_eq!(plan.aur_strata.len(), 1);
        assert_eq!(plan.aur_strata[0], vec!["helper-pkg".to_string()]);
    }

    // ---- direct/transitive partitioning ---------------------------------

    #[test]
    fn user_named_repo_target_is_direct_even_when_also_dep() {
        // foo is a repo pkg and also a makedep of an AUR pkg a. User asked
        // for foo explicitly → direct_repo (not transitive_repo).
        let plan = run(&["foo", "a"], vec![entry("a", &[], &["foo"])], &["foo"]).unwrap();
        assert_eq!(plan.direct_repo, vec!["foo".to_string()]);
        assert!(plan.transitive_repo.is_empty());
    }

    #[test]
    fn unsolicited_repo_dep_is_transitive() {
        let plan = run(&["a"], vec![entry("a", &[], &["foo"])], &["foo"]).unwrap();
        assert!(plan.direct_repo.is_empty());
        assert_eq!(plan.transitive_repo, vec!["foo".to_string()]);
    }

    // ---- cycles ---------------------------------------------------------

    #[test]
    fn cycle_in_makedepends_errors() {
        let err = run(
            &["a"],
            vec![entry("a", &[], &["b"]), entry("b", &[], &["a"])],
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, Error::Resolve(_)));
    }

    #[test]
    fn cycle_in_runtime_depends_errors_too() {
        // Strata wouldn't see this cycle (it's not on make edges), but the
        // full-dep cycle check catches it.
        let err = run(
            &["a"],
            vec![entry("a", &["b"], &[]), entry("b", &["a"], &[])],
            &[],
        )
        .unwrap_err();
        assert!(matches!(err, Error::Resolve(_)));
    }

    // ---- direct_targets bookkeeping -------------------------------------

    #[test]
    fn direct_targets_records_user_input_verbatim() {
        let plan = run(&["foo", "bar>=2"], vec![], &["foo", "bar"]).unwrap();
        assert!(plan.direct_targets.contains("foo"));
        assert!(plan.direct_targets.contains("bar>=2"));
    }

    // ---- aur_order() convenience ----------------------------------------

    #[test]
    fn aur_order_flattens_strata_preserving_order() {
        let plan = run(
            &["a"],
            vec![
                entry("a", &[], &["b"]),
                entry("b", &[], &["c"]),
                entry("c", &[], &[]),
            ],
            &[],
        )
        .unwrap();
        assert_eq!(plan.aur_order(), vec!["c", "b", "a"]);
    }
}
