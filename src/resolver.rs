//! Recursive dependency resolution: targets → ordered Plan.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::index::IndexFile;
use crate::index::secondary::{self, Secondary};
use crate::names::{PkgBase, PkgName, PkgTarget};
use crate::pacman::alpm_db::PacmanIndex;
use std::collections::{BTreeSet, HashMap, HashSet};
use tracing::{debug, info, instrument};

pub mod classify;
pub mod pkgbase_expand;
pub mod topo;

pub use classify::{Source, classify};
pub use pkgbase_expand::{ExpandedTargets, expand_pkgbase_targets};

/// Resolved install plan partitioned by source.
///
/// Field types use the typed [`PkgBase`]/[`PkgName`] newtypes wherever the
/// identity is unambiguous — pkgbase-keyed maps, pkgbase strata, pkgname
/// selections. `direct_repo` / `transitive_repo` / `direct_targets` stay
/// `String` because their contents are deliberately mixed (virtual provides,
/// version-suffixed names, freeform pacman targets).
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
    pub aur_strata: Vec<Vec<PkgBase>>,
    /// AUR pkgbase → AUR pkgbase makedeps (the same edges fed to
    /// `topo::strata`). Lets the build pipeline propagate failures: if A
    /// fails, anything with A in its closure is skipped instead of being
    /// attempted with a missing dep.
    pub aur_make_edges: HashMap<PkgBase, Vec<PkgBase>>,
    /// User-requested top-level targets — pre-classification (could be
    /// pkgname / pkgbase / virtual / version-suffixed), so typed as
    /// [`PkgTarget`]. Used to flip a `.pkg.tar.zst`'s install reason
    /// from `--asdeps` to Explicit when the built pkgname matches a
    /// user target — see [`crate::names::PkgTargetSetExt::contains_pkgname`].
    pub direct_targets: HashSet<PkgTarget>,
    /// Per-pkgbase pkgname subset for split-package targets where the user
    /// chose to install only some pkgnames. makepkg has no flag to limit
    /// which pkgnames it builds (`package_*()` is all-or-nothing for a
    /// pkgbase), so the build always produces every pkgname's
    /// `.pkg.tar.zst`; this map drives the *install* filter only — only
    /// listed pkgnames are fed into the final `pacman -U`. Pkgbases absent
    /// from the map default to "install everything".
    pub pkgname_selections: HashMap<PkgBase, Vec<PkgName>>,
    /// AUR pkgbases the user named directly, as opposed to ones dragged in
    /// as another pkgbase's dep. The repo side gets this split for free
    /// (`direct_repo` vs. `transitive_repo`), but `aur_strata` mixes both;
    /// this set is the AUR complement so [`Plan::only_requested`] can tell
    /// an all-explicit plan apart from one that pulls in unrequested builds.
    pub direct_aur: HashSet<PkgBase>,
    /// pkgbase → user's intended counterpart pkgname (see
    /// [`crate::build::Target::hint`]). Populated by
    /// [`expand_pkgbase_targets`] for any target rewritten via the
    /// pkgname / provides path, and by `-Syu` for picker-supplied hints
    /// on pkgbase targets. `prepare_one` feeds the hint to
    /// [`crate::pacman::alpm_db::PacmanIndex::counterpart_with_hint`] so
    /// review labels and diff-base lookups land on the installed pkg the
    /// user actually meant, not the first one that happens to match the
    /// pkgbase's provides list.
    pub counterpart_hints: HashMap<PkgBase, PkgName>,
}

impl Plan {
    /// Flatten all AUR pkgbases in build order. Convenience for code paths
    /// that don't care about stratum boundaries (counts, displays, …).
    pub fn aur_order(&self) -> Vec<PkgBase> {
        self.aur_strata.iter().flatten().cloned().collect()
    }

    /// True when every package the plan would install was named by the user:
    /// no repo dependencies pulled in (`transitive_repo` empty) and every AUR
    /// pkgbase was a direct target (none dragged in as another's dep). The
    /// build pipeline skips its "Proceed with installation?" prompt in this
    /// case — the plan table just echoes the user's own request, so the
    /// confirm only earns its place when there are unrequested packages
    /// (deps or makedepends) to disclose. The sudo "Continue?" gate and the
    /// per-pkgbase AUR review prompts still apply regardless.
    pub fn only_requested(&self) -> bool {
        self.transitive_repo.is_empty()
            && self
                .aur_strata
                .iter()
                .flatten()
                .all(|pb| self.direct_aur.contains(pb))
    }

    /// What the plan says about one pkgbase: the pkgbase itself plus the two
    /// optional per-pkgbase decisions keyed off it (partial-split selection
    /// and counterpart hint). Borrows from `self`, so the returned value is
    /// valid for the lifetime of the plan.
    pub fn pkgbase_plan<'a>(&'a self, pkgbase: &'a PkgBase) -> PkgbasePlan<'a> {
        PkgbasePlan {
            pkgbase,
            selection: self.pkgname_selections.get(pkgbase).map(Vec::as_slice),
            hint: self.counterpart_hints.get(pkgbase),
        }
    }
}

/// One pkgbase plus the [`Plan`]'s decisions about it. Returned by
/// [`Plan::pkgbase_plan`]; consumed by `build::prepare_one`.
pub struct PkgbasePlan<'a> {
    pub pkgbase: &'a PkgBase,
    pub selection: Option<&'a [PkgName]>,
    pub hint: Option<&'a PkgName>,
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
    let mut visited_aur: BTreeSet<PkgBase> = BTreeSet::new();
    let mut missing: Vec<String> = Vec::new();
    // Separate graphs:
    //   * `all_edges` covers runtime + build-time deps; used for cycle
    //     detection so a cycle through plain `depends` is still rejected.
    //   * `make_edges` covers only build-time deps (makedepends +
    //     checkdepends) that resolve to AUR pkgbases; drives strata so
    //     each pkg's build-time AUR deps are installed before it runs.
    //
    // Keys are typed `PkgBase` (the graph node identity); values stay
    // `Vec<String>` because they're raw dep expressions before pkgname →
    // pkgbase resolution. Post-resolution `make_edges_resolved` has
    // `Vec<PkgBase>` on both sides.
    let mut all_edges: HashMap<PkgBase, Vec<String>> = HashMap::new();
    let mut make_edges: HashMap<PkgBase, Vec<String>> = HashMap::new();
    let mut pkgname_to_pkgbase: HashMap<PkgName, PkgBase> = HashMap::new();

    let direct_set: HashSet<String> = targets
        .iter()
        .map(|t| secondary::strip_version_constraint(t).to_owned())
        .collect();
    for t in targets {
        // Widen the raw CLI/picker string into a typed `PkgTarget` — the
        // boundary where unclassified user input enters the typed graph.
        plan.direct_targets.insert(PkgTarget::from(t.as_str()));
    }

    let mut queue: Vec<(String, bool)> = targets.iter().map(|t| (t.clone(), true)).collect();
    while let Some((target, is_direct)) = queue.pop() {
        let bare = secondary::strip_version_constraint(&target).to_owned();
        let source = resolve_target_source(by, pac, &bare, is_direct);
        match source {
            Source::Installed(concrete) => {
                debug!(target = %bare, %concrete, "already satisfied (installed)");
            }
            Source::Repo(concrete) => {
                // direct_set is keyed on the **user-typed** name so explicit
                // `gitaur -S cargo` flips the resolved provider (`rust`) into
                // direct_repo even when it also appears as another pkg's dep.
                let direct = is_direct || direct_set.contains(&bare);
                let bucket = if direct {
                    &mut plan.direct_repo
                } else {
                    &mut plan.transitive_repo
                };
                if !bucket.iter().any(|s| s == &concrete) {
                    bucket.push(concrete);
                }
            }
            Source::Aur(entry_idx) => {
                let entry = &idx.entries[entry_idx];
                let pkgbase = entry.pkgbase.clone();
                // Record direct-ness before the visited-dedup `continue`: the
                // queue is LIFO, so a pkgbase can be popped as a dep before
                // the direct target that also names it. `direct_set` (keyed on
                // the user-typed string) backstops the `is_direct` flag for
                // that ordering, mirroring the repo branch above.
                if is_direct || direct_set.contains(&bare) {
                    plan.direct_aur.insert(pkgbase.clone());
                }
                for pkg in &entry.pkgnames {
                    pkgname_to_pkgbase.insert(pkg.name.clone(), pkgbase.clone());
                }
                if !visited_aur.insert(pkgbase.clone()) {
                    continue;
                }
                let runtime: Vec<String> = entry
                    .depends
                    .iter()
                    .map(|d| secondary::strip_version_constraint(d).to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
                let build_time: Vec<String> = entry
                    .makedepends
                    .iter()
                    .chain(entry.checkdepends.iter())
                    .map(|d| secondary::strip_version_constraint(d).to_owned())
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
    topo::sort(&all_edges, &visited_aur)?;

    // Rewrite make_edges entries from pkgnames → AUR pkgbases. Drop entries
    // pointing at repo/installed targets (irrelevant for build ordering).
    // `HashMap<PkgName, PkgBase>::get(&str)` via `Borrow<str>` saves us
    // a PkgName allocation per dep lookup.
    let make_edges_resolved: HashMap<PkgBase, Vec<PkgBase>> = make_edges
        .into_iter()
        .map(|(pkgbase, deps)| {
            let resolved: Vec<PkgBase> = deps
                .into_iter()
                .filter_map(|d| pkgname_to_pkgbase.get(d.as_str()).cloned())
                .filter(|pb| pb != &pkgbase) // self-edges from split pkgs
                .collect();
            (pkgbase, resolved)
        })
        .collect();

    plan.aur_strata = topo::strata(&make_edges_resolved, &visited_aur)?;
    plan.aur_make_edges = make_edges_resolved;

    // Cross-bucket dedup: a concrete pkg may have landed in direct_repo via
    // one alias (e.g. user typed `rust`) and in transitive_repo via another
    // (an AUR makedep `cargo` → `rust`). Direct wins — drop the transitive
    // copy so the install isn't double-listed and explicit-vs-asdeps stays
    // consistent.
    let direct_set: HashSet<&str> = plan.direct_repo.iter().map(String::as_str).collect();
    plan.transitive_repo
        .retain(|n| !direct_set.contains(n.as_str()));

    info!(
        direct_repo = plan.direct_repo.len(),
        transitive_repo = plan.transitive_repo.len(),
        aur_strata = plan.aur_strata.len(),
        aur_total = plan.aur_strata.iter().map(Vec::len).sum::<usize>(),
        "plan resolved",
    );
    Ok(plan)
}

/// Wrap [`classify`] with a rebuild override for direct targets: when the
/// user explicitly named a pkg that classifies as `Installed` and that name
/// also exists in the AUR index, return `Source::Aur` so the build path
/// picks it up. The classifier stops at pacman precedence and can't see
/// version, so an outdated installed AUR pkg would otherwise be silently
/// dropped — breaking `-Syu`'s AUR half and `-S name` on an already-
/// installed AUR pkg. Transitive deps keep the default behavior; a satisfied
/// dep is not a rebuild trigger.
fn resolve_target_source(
    by: Option<&Secondary>,
    pac: &PacmanIndex,
    bare: &str,
    is_direct: bool,
) -> Source {
    let source = classify(by, pac, bare);
    if !is_direct {
        return source;
    }
    let (Source::Installed(_), Some(by)) = (&source, by) else {
        return source;
    };
    let aur_hit = by
        .by_name
        .get(bare)
        .copied()
        .or_else(|| by.by_provides.get(bare).and_then(|v| v.first().copied()))
        .or_else(|| by.by_pkgbase.get(bare).copied());
    aur_hit.map_or(source, |i| Source::Aur(i as usize))
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
        use crate::index::schema::Pkgname;
        IndexEntry {
            pkgbase: pkgbase.into(),
            pkgnames: pkgnames
                .iter()
                .map(|s| Pkgname {
                    name: (*s).into(),
                    provides: Vec::new(),
                    pkgdesc: None,
                })
                .collect(),
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
            pac.sync_versions.insert((*n).into(), "1.0-1".into());
        }
        let cfg = default_config();
        let targets: Vec<String> = targets.iter().map(|s| (*s).to_owned()).collect();
        resolve(&cfg, &idx, Some(&by), &pac, &targets)
    }

    // ---- repo-only paths -------------------------------------------------

    #[test]
    fn pure_repo_target_lands_in_direct_repo() {
        let plan = run(&["foo"], vec![], &["foo"]).unwrap();
        assert_eq!(plan.direct_repo, vec!["foo".to_owned()]);
        assert!(plan.transitive_repo.is_empty());
        assert!(plan.aur_strata.is_empty());
    }

    #[test]
    fn version_constraint_on_repo_target_strips() {
        let plan = run(&["foo>=1.2"], vec![], &["foo"]).unwrap();
        assert_eq!(plan.direct_repo, vec!["foo".to_owned()]);
    }

    #[test]
    fn missing_target_errors() {
        let err = run(&["nope"], vec![], &[]).unwrap_err();
        match err {
            Error::UnknownTargets(s) => assert_eq!(s, "nope"),
            other => panic!("expected UnknownTargets, got {other:?}"),
        }
    }

    // ---- only_requested: when the plan adds nothing the user didn't name -

    #[test]
    fn only_requested_for_pure_repo_target() {
        let plan = run(&["foo"], vec![], &["foo"]).unwrap();
        assert!(plan.only_requested());
    }

    #[test]
    fn only_requested_false_when_repo_dep_pulled_in() {
        // AUR `a` drags in repo `bash` as a makedep → unrequested repo pkg.
        let plan = run(&["a"], vec![entry("a", &[], &["bash"])], &["bash"]).unwrap();
        assert!(!plan.only_requested());
    }

    #[test]
    fn only_requested_for_named_aur_with_no_deps() {
        let plan = run(&["a"], vec![entry("a", &[], &[])], &[]).unwrap();
        assert!(plan.only_requested());
    }

    #[test]
    fn only_requested_false_when_aur_makedep_pkgbase_pulled_in() {
        // `a` makedepends on AUR `b`; `b` is built but never named.
        let plan = run(
            &["a"],
            vec![entry("a", &[], &["b"]), entry("b", &[], &[])],
            &[],
        )
        .unwrap();
        assert!(!plan.only_requested());
        assert!(plan.direct_aur.contains("a"));
        assert!(!plan.direct_aur.contains("b"));
    }

    #[test]
    fn only_requested_when_both_aur_pkgbases_named() {
        // Same graph, but the user names `b` too → no unrequested pkg.
        let plan = run(
            &["a", "b"],
            vec![entry("a", &[], &["b"]), entry("b", &[], &[])],
            &[],
        )
        .unwrap();
        assert!(plan.only_requested());
    }

    // ---- single AUR pkg --------------------------------------------------

    #[test]
    fn aur_with_no_deps_single_stratum() {
        let plan = run(&["a"], vec![entry("a", &[], &[])], &[]).unwrap();
        assert_eq!(plan.aur_strata, vec![vec!["a".to_owned()]]);
    }

    #[test]
    fn aur_with_repo_makedep_pulls_transitive_repo() {
        let plan = run(&["a"], vec![entry("a", &[], &["bash"])], &["bash"]).unwrap();
        assert_eq!(plan.aur_strata, vec![vec!["a".to_owned()]]);
        assert_eq!(plan.transitive_repo, vec!["bash".to_owned()]);
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
        assert_eq!(s0, vec!["a".to_owned(), "b".to_owned()]);
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
            vec![vec!["b".to_owned()], vec!["a".to_owned()]]
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
                vec!["c".to_owned()],
                vec!["b".to_owned()],
                vec!["a".to_owned()],
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
        assert_eq!(plan.aur_strata[0], vec!["a".to_owned()]);
        let mut mid = plan.aur_strata[1].clone();
        mid.sort();
        assert_eq!(mid, vec!["b".to_owned(), "c".to_owned()]);
        assert_eq!(plan.aur_strata[2], vec!["d".to_owned()]);
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
            vec![vec!["b".to_owned()], vec!["a".to_owned()]]
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
            vec![vec!["helper-pkg".to_owned()], vec!["client".to_owned()]]
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
        assert_eq!(plan.aur_strata[0], vec!["helper-pkg".to_owned()]);
    }

    // ---- direct/transitive partitioning ---------------------------------

    #[test]
    fn user_named_repo_target_is_direct_even_when_also_dep() {
        // foo is a repo pkg and also a makedep of an AUR pkg a. User asked
        // for foo explicitly → direct_repo (not transitive_repo).
        let plan = run(&["foo", "a"], vec![entry("a", &[], &["foo"])], &["foo"]).unwrap();
        assert_eq!(plan.direct_repo, vec!["foo".to_owned()]);
        assert!(plan.transitive_repo.is_empty());
    }

    #[test]
    fn unsolicited_repo_dep_is_transitive() {
        let plan = run(&["a"], vec![entry("a", &[], &["foo"])], &["foo"]).unwrap();
        assert!(plan.direct_repo.is_empty());
        assert_eq!(plan.transitive_repo, vec!["foo".to_owned()]);
    }

    // ---- provides resolution: the `paru` regression --------------------

    /// AUR `paru` makedepends on the virtual `cargo` and `libalpm.so`.
    /// Those are not packages — `rust` provides `cargo` and `pacman`
    /// provides `libalpm.so`. The plan must list the concrete pkgnames
    /// with their versions, never the virtuals.
    #[test]
    fn aur_makedep_virtual_provides_resolves_to_concrete() {
        let idx = IndexFile {
            entries: vec![entry("paru", &[], &["cargo", "libalpm.so"])],
            ..IndexFile::empty()
        };
        let by = Secondary::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.sync_versions.insert("rust".into(), "1.80.0-1".into());
        pac.sync_providers
            .insert("cargo".into(), vec!["rust".into()]);
        pac.sync_versions.insert("pacman".into(), "6.1.0-1".into());
        pac.sync_providers
            .insert("libalpm.so".into(), vec!["pacman".into()]);
        let cfg = default_config();
        let plan = resolve(&cfg, &idx, Some(&by), &pac, &["paru".to_owned()]).unwrap();

        assert_eq!(plan.aur_strata, vec![vec!["paru".to_owned()]]);
        let mut t = plan.transitive_repo.clone();
        t.sort();
        assert_eq!(t, vec!["pacman".to_owned(), "rust".to_owned()]);
        assert!(plan.direct_repo.is_empty());
    }

    /// When a provider is already installed, the dep is already satisfied —
    /// pacman --needed would no-op — so the resolver must drop it entirely
    /// rather than show a phantom row in the plan.
    #[test]
    fn aur_makedep_provider_already_installed_is_dropped() {
        let idx = IndexFile {
            entries: vec![entry("paru", &[], &["cargo"])],
            ..IndexFile::empty()
        };
        let by = Secondary::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.installed.insert("rust".into(), "1.80.0-1".into());
        pac.installed_providers
            .insert("cargo".into(), vec!["rust".into()]);
        // rust is also in sync, but installed wins.
        pac.sync_versions.insert("rust".into(), "1.80.0-1".into());
        pac.sync_providers
            .insert("cargo".into(), vec!["rust".into()]);
        let cfg = default_config();
        let plan = resolve(&cfg, &idx, Some(&by), &pac, &["paru".to_owned()]).unwrap();

        assert_eq!(plan.aur_strata, vec![vec!["paru".to_owned()]]);
        assert!(plan.transitive_repo.is_empty());
        assert!(plan.direct_repo.is_empty());
    }

    /// A concrete pkg may land in `direct_repo` via one alias (`rust`) AND in
    /// `transitive_repo` via another (a dep `cargo` → `rust`). Direct wins —
    /// the cross-bucket dedup pass must drop the transitive copy so the
    /// install argv isn't double-listed.
    #[test]
    fn provides_dedup_prefers_direct_over_transitive() {
        let idx = IndexFile {
            entries: vec![entry("paru", &[], &["cargo"])],
            ..IndexFile::empty()
        };
        let by = Secondary::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.sync_versions.insert("rust".into(), "1.80.0-1".into());
        pac.sync_providers
            .insert("cargo".into(), vec!["rust".into()]);
        let cfg = default_config();
        let plan = resolve(
            &cfg,
            &idx,
            Some(&by),
            &pac,
            &["rust".to_owned(), "paru".to_owned()],
        )
        .unwrap();

        assert_eq!(plan.direct_repo, vec!["rust".to_owned()]);
        assert!(
            plan.transitive_repo.is_empty(),
            "rust must not appear in both buckets, got transitive {:?}",
            plan.transitive_repo
        );
    }

    // ---- direct-rebuild override ----------------------------------------

    /// A direct target that's already installed but also lives in the AUR
    /// index must rebuild (route to `aur_strata`), not get dropped as satisfied.
    /// This is the `-Syu` AUR upgrade path and `-S name` on an installed AUR
    /// pkg — the classifier can't see version, so the resolver overrides.
    #[test]
    fn installed_direct_aur_target_routes_to_rebuild() {
        let idx = IndexFile {
            entries: vec![entry("brave-bin", &[], &[])],
            ..IndexFile::empty()
        };
        let by = Secondary::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.installed
            .insert("brave-bin".into(), "1:1.90.121-1".into());
        let cfg = default_config();
        let plan = resolve(&cfg, &idx, Some(&by), &pac, &["brave-bin".to_owned()]).unwrap();
        assert_eq!(plan.aur_strata, vec![vec!["brave-bin".to_owned()]]);
    }

    /// Transitive deps that are already installed must stay dropped — only
    /// direct targets get the rebuild override. A makedep `helper` that's
    /// installed shouldn't trigger a needless rebuild of `helper`.
    #[test]
    fn installed_transitive_aur_dep_is_still_dropped() {
        let idx = IndexFile {
            entries: vec![entry("client", &[], &["helper"]), entry("helper", &[], &[])],
            ..IndexFile::empty()
        };
        let by = Secondary::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.installed.insert("helper".into(), "1.0-1".into());
        let cfg = default_config();
        let plan = resolve(&cfg, &idx, Some(&by), &pac, &["client".to_owned()]).unwrap();
        // Only client should be in the build plan; helper stays satisfied.
        assert_eq!(plan.aur_strata, vec![vec!["client".to_owned()]]);
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
