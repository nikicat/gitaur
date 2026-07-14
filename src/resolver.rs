//! Recursive dependency resolution: targets → ordered Plan.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::index::lookup::{self, Lookup};
use crate::index::{IndexEntry, IndexFile};
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

/// How a target entered the resolver's work queue: named by the user
/// (`Direct`) or pulled in as another package's dependency (`Dep`). Drives
/// the explicit-vs-`--asdeps` split (`direct_repo`/`transitive_repo`,
/// `direct_aur`) and gates the direct-target rebuild override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    Direct,
    Dep,
}

impl Origin {
    /// Whether `target` should count as user-requested. True when it was
    /// queued `Direct`, or when its bare name is in `direct_set` — the user
    /// typed it explicitly even though the LIFO queue happened to pop it as a
    /// dependency first (`aurox -S cargo` where `cargo` is also an AUR pkg's
    /// makedep). The `direct_set` backstop is only relevant to the bucketing
    /// split, not the rebuild override (see [`resolve_target_source`]).
    fn is_direct(self, target: &PkgTarget, direct_set: &HashSet<PkgTarget>) -> bool {
        self == Self::Direct || direct_set.contains(target.bare())
    }
}

/// Resolve `targets` against the index + pacman DBs into a [`Plan`].
///
/// With no AUR data in play `idx`/`by` are simply *empty* (the loader seam's
/// pacman-only view); classification then degenerates to pacman-only and any
/// unknown name short-circuits to [`Source::Missing`].
#[instrument(skip(_cfg, idx, by, pac), fields(targets = targets.len()))]
pub fn resolve(
    _cfg: &Config,
    idx: &IndexFile,
    by: &Lookup,
    pac: &PacmanIndex,
    targets: &[String],
) -> Result<Plan> {
    let mut plan = Plan::default();
    let mut visited_aur: BTreeSet<PkgBase> = BTreeSet::new();
    // Concrete repo pkgnames whose `depends` we've already expanded. Repo
    // deps can't cycle, but a package reached via several aliases (or as both
    // a direct target and a dep) must only be walked once.
    let mut visited_repo: HashSet<PkgName> = HashSet::new();
    let mut missing: Vec<String> = Vec::new();
    // Separate graphs:
    //   * `all_edges` covers runtime + build-time deps; used for cycle
    //     detection so a cycle through plain `depends` is still rejected.
    //   * `make_edges` covers only build-time deps (makedepends +
    //     checkdepends) that resolve to AUR pkgbases; drives strata so
    //     each pkg's build-time AUR deps are installed before it runs.
    //
    // Keys are typed `PkgBase` (the graph node identity); values are
    // `PkgTarget` — unclassified dep expressions before pkgname → pkgbase
    // resolution. Post-resolution `make_edges_resolved` has `Vec<PkgBase>`
    // on both sides.
    let mut all_edges: HashMap<PkgBase, Vec<PkgTarget>> = HashMap::new();
    let mut make_edges: HashMap<PkgBase, Vec<PkgTarget>> = HashMap::new();
    let mut pkgname_to_pkgbase: HashMap<PkgName, PkgBase> = HashMap::new();

    let direct_set: HashSet<PkgTarget> = targets
        .iter()
        .map(|t| PkgTarget::from(lookup::strip_version_constraint(t)))
        .collect();
    for t in targets {
        // Widen the raw CLI/picker string into a typed `PkgTarget` — the
        // boundary where unclassified user input enters the typed graph.
        plan.direct_targets.insert(PkgTarget::from(t.as_str()));
    }

    let mut queue: Vec<(PkgTarget, Origin)> = targets
        .iter()
        .map(|t| (PkgTarget::from(t.as_str()), Origin::Direct))
        .collect();
    while let Some((target, origin)) = queue.pop() {
        let bare = target.bare();
        let source = resolve_target_source(by, pac, bare, origin);
        match source {
            Source::Installed(concrete) => {
                debug!(target = %bare, %concrete, "already satisfied (installed)");
            }
            Source::Repo(concrete) => {
                // Expand this repo package's deps into the BFS so any not-yet-
                // installed repo dependency surfaces in the plan instead of
                // being pulled in silently by the final `pacman -S`. Walk once
                // per concrete pkg (repo deps can't cycle, but aliases repeat).
                if visited_repo.insert(concrete.clone()) {
                    enqueue_repo_deps(pac, &concrete, &mut queue);
                }
                // direct_set is keyed on the **user-typed** name so explicit
                // `aurox -S cargo` flips the resolved provider (`rust`) into
                // direct_repo even when it also appears as another pkg's dep.
                let bucket = if origin.is_direct(&target, &direct_set) {
                    &mut plan.direct_repo
                } else {
                    &mut plan.transitive_repo
                };
                if !bucket.iter().any(|s| concrete == *s) {
                    bucket.push(concrete.into_inner());
                }
            }
            Source::Aur(entry_idx) => {
                let entry = &idx.entries[entry_idx];
                let pkgbase = entry.pkgbase.clone();
                // Record direct-ness before the visited-dedup `continue`: the
                // queue is LIFO, so a pkgbase can be popped as a dep before
                // the direct target that also names it. `direct_set` (keyed on
                // the user-typed string) backstops the queue `Origin` for that
                // ordering, mirroring the repo branch above.
                if origin.is_direct(&target, &direct_set) {
                    plan.direct_aur.insert(pkgbase.clone());
                }
                for pkg in &entry.pkgnames {
                    pkgname_to_pkgbase.insert(pkg.name.clone(), pkgbase.clone());
                }
                if !visited_aur.insert(pkgbase.clone()) {
                    continue;
                }
                // `all` (runtime + build-time) feeds the cycle graph and the
                // BFS; `build_time` alone is staged for strata and rewritten
                // pkgname → pkgbase after the BFS (needs the full name map).
                let (all, build_time) = entry_dep_exprs(entry);
                // A dep that names this pkgbase itself (a package that lists
                // itself in `depends`, or a split sibling whose pkgname equals
                // the pkgbase) is a self-edge, not a dependency cycle — drop it
                // before the `topo::sort` check, mirroring the self-edge filter
                // `resolve_make_edges` already applies to the strata graph. The
                // BFS still sees the self-dep, but it dedups against
                // `visited_aur` on the next pop, so nothing is lost.
                let cycle_edges: Vec<PkgTarget> = all
                    .iter()
                    .filter(|d| !d.refers_to(&pkgbase))
                    .cloned()
                    .collect();
                all_edges.insert(pkgbase.clone(), cycle_edges);
                make_edges.insert(pkgbase.clone(), build_time);
                queue.extend(all.into_iter().map(|d| (d, Origin::Dep)));
            }
            Source::Missing => missing.push(bare.to_owned()),
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

    let make_edges_resolved = resolve_make_edges(make_edges, &pkgname_to_pkgbase);
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

/// Expand a repo package's `depends` into the BFS `queue` as `Origin::Dep`
/// targets. Each is classified on its next pop: already-installed deps drop
/// out as `Installed`, sync deps land in `transitive_repo`, virtuals resolve
/// to their provider. Surfacing them here is what lets the plan show — and
/// [`Plan::only_requested`] gate on — deps the final `pacman -S` would
/// otherwise pull in silently.
fn enqueue_repo_deps(pac: &PacmanIndex, concrete: &PkgName, queue: &mut Vec<(PkgTarget, Origin)>) {
    for dep in pac.sync_depends(concrete) {
        if !dep.bare().is_empty() {
            queue.push((dep.clone(), Origin::Dep));
        }
    }
}

/// Split an AUR entry's dependency arrays into the two dep-reference lists
/// the BFS needs: `(all, build_time)`. `all` = runtime `depends` + build-time
/// (`makedepends` + `checkdepends`); `build_time` is that build-time subset
/// alone. Version constraints are stripped off the dep-specs (so the bare
/// name indexes the graph directly), and empties are dropped.
fn entry_dep_exprs(entry: &IndexEntry) -> (Vec<PkgTarget>, Vec<PkgTarget>) {
    let widen = |d: &PkgTarget| {
        let bare = d.bare();
        (!bare.is_empty()).then(|| PkgTarget::from(bare))
    };
    let build_time: Vec<PkgTarget> = entry
        .makedepends
        .iter()
        .chain(entry.checkdepends.iter())
        .filter_map(widen)
        .collect();
    // `all` prepends runtime `depends` to the build-time set; the latter is
    // already materialised, so chain its clones rather than collecting twice.
    let all = entry
        .depends
        .iter()
        .filter_map(widen)
        .chain(build_time.iter().cloned())
        .collect();
    (all, build_time)
}

/// Rewrite staged build-time deps from pkgnames into AUR pkgbase edges:
/// resolve each dep to its owning pkgbase, dropping deps that point at
/// repo/installed targets (irrelevant to build order) and self-edges from
/// split pkgs. The `HashMap<PkgName, PkgBase>::get(&str)` lookup rides
/// `Borrow<str>` to skip a `PkgName` allocation per dep.
fn resolve_make_edges(
    make_edges: HashMap<PkgBase, Vec<PkgTarget>>,
    pkgname_to_pkgbase: &HashMap<PkgName, PkgBase>,
) -> HashMap<PkgBase, Vec<PkgBase>> {
    make_edges
        .into_iter()
        .map(|(pkgbase, deps)| {
            let resolved: Vec<PkgBase> = deps
                .into_iter()
                .filter_map(|d| pkgname_to_pkgbase.get(d.bare()).cloned())
                .filter(|pb| pb != &pkgbase)
                .collect();
            (pkgbase, resolved)
        })
        .collect()
}

/// Wrap [`classify`] with a rebuild override for direct targets: when the
/// user explicitly named a pkg that classifies as `Installed` and that name
/// also exists in the AUR index, return `Source::Aur` so the build path
/// picks it up. The classifier stops at pacman precedence and can't see
/// version, so an outdated installed AUR pkg would otherwise be silently
/// dropped — breaking `-Syu`'s AUR half and `-S name` on an already-
/// installed AUR pkg. Transitive deps keep the default behavior; a satisfied
/// dep is not a rebuild trigger — so only `Origin::Direct` (the raw queue
/// flag, not the `direct_set` backstop) arms the override.
fn resolve_target_source(by: &Lookup, pac: &PacmanIndex, bare: &str, origin: Origin) -> Source {
    let source = classify(by, pac, bare);
    if origin != Origin::Direct {
        return source;
    }
    let Source::Installed(_) = &source else {
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
        let by = Lookup::build(&idx);
        let mut pac = PacmanIndex::default();
        for n in repo {
            pac.sync_versions.insert((*n).into(), "1.0-1".into());
        }
        let cfg = default_config();
        let targets: Vec<String> = targets.iter().map(|s| (*s).to_owned()).collect();
        resolve(&cfg, &idx, &by, &pac, &targets)
    }

    /// Resolve `targets` against a caller-built [`PacmanIndex`] and no AUR
    /// index — the seam for repo→repo dependency-walk tests, which set up
    /// `sync_versions` / `sync_depends` / `installed` with typed values.
    fn resolve_repo(pac: &PacmanIndex, targets: &[&str]) -> Plan {
        let idx = IndexFile::empty();
        let by = Lookup::build(&idx);
        let cfg = default_config();
        let targets: Vec<String> = targets.iter().map(|s| (*s).to_owned()).collect();
        resolve(&cfg, &idx, &by, pac, &targets).unwrap()
    }

    /// Register a sync package with its dependency list, typed at the seam:
    /// `PkgName` key, `PkgTarget` deps. Keeps the per-test setup declarative.
    fn sync_pkg(pac: &mut PacmanIndex, name: &str, deps: &[&str]) {
        pac.sync_versions.insert(name.into(), "1.0-1".into());
        pac.sync_depends.insert(
            name.into(),
            deps.iter().map(|d| PkgTarget::from(*d)).collect(),
        );
    }

    /// Sorted clone of `transitive_repo` — the BFS pushes in pop order, so
    /// tests assert membership, not the incidental ordering.
    fn sorted_transitive(plan: &Plan) -> Vec<String> {
        let mut t = plan.transitive_repo.clone();
        t.sort();
        t
    }

    // ---- repo dependency tree --------------------------------------------

    #[test]
    fn repo_target_pulls_uninstalled_repo_dep() {
        // `foo` depends on repo `bar`; nothing installed. `bar` is an
        // unrequested repo dep → transitive_repo, and the prompt must fire.
        let mut pac = PacmanIndex::default();
        sync_pkg(&mut pac, "foo", &["bar"]);
        sync_pkg(&mut pac, "bar", &[]);
        let plan = resolve_repo(&pac, &["foo"]);
        assert_eq!(plan.direct_repo, vec!["foo".to_owned()]);
        assert_eq!(plan.transitive_repo, vec!["bar".to_owned()]);
        assert!(!plan.only_requested());
    }

    #[test]
    fn repo_dep_already_installed_is_dropped() {
        // Same graph, but `bar` is already installed → satisfied, dropped from
        // the plan. Nothing unrequested remains, so the prompt is skipped.
        let mut pac = PacmanIndex::default();
        sync_pkg(&mut pac, "foo", &["bar"]);
        sync_pkg(&mut pac, "bar", &[]);
        pac.installed.insert("bar".into(), "1.0-1".into());
        let plan = resolve_repo(&pac, &["foo"]);
        assert_eq!(plan.direct_repo, vec!["foo".to_owned()]);
        assert!(plan.transitive_repo.is_empty());
        assert!(plan.only_requested());
    }

    #[test]
    fn repo_deps_walk_transitively() {
        // foo → bar → baz, all repo, none installed. The walk must reach the
        // whole closure, not just the direct dep.
        let mut pac = PacmanIndex::default();
        sync_pkg(&mut pac, "foo", &["bar"]);
        sync_pkg(&mut pac, "bar", &["baz"]);
        sync_pkg(&mut pac, "baz", &[]);
        let plan = resolve_repo(&pac, &["foo"]);
        assert_eq!(plan.direct_repo, vec!["foo".to_owned()]);
        assert_eq!(
            sorted_transitive(&plan),
            vec!["bar".to_owned(), "baz".to_owned()]
        );
        assert!(!plan.only_requested());
    }

    #[test]
    fn user_named_repo_dep_stays_direct() {
        // User names both `foo` and its dep `bar` → both explicit, nothing
        // unrequested, prompt skipped. `bar` is reached as a dep too, but the
        // direct_set backstop keeps it in direct_repo, not transitive_repo.
        let mut pac = PacmanIndex::default();
        sync_pkg(&mut pac, "foo", &["bar"]);
        sync_pkg(&mut pac, "bar", &[]);
        let plan = resolve_repo(&pac, &["foo", "bar"]);
        let mut direct = plan.direct_repo.clone();
        direct.sort();
        assert_eq!(direct, vec!["bar".to_owned(), "foo".to_owned()]);
        assert!(plan.transitive_repo.is_empty());
        assert!(plan.only_requested());
    }

    #[test]
    fn repo_dep_via_virtual_provide_resolves_to_concrete() {
        // `foo` depends on the virtual `libbar.so`, provided by sync pkg
        // `barlib`. The plan must list the concrete provider, not the virtual.
        let mut pac = PacmanIndex::default();
        sync_pkg(&mut pac, "foo", &["libbar.so"]);
        pac.sync_versions.insert("barlib".into(), "2.0-1".into());
        pac.sync_providers
            .insert("libbar.so".into(), vec!["barlib".into()]);
        let plan = resolve_repo(&pac, &["foo"]);
        assert_eq!(plan.direct_repo, vec!["foo".to_owned()]);
        assert_eq!(plan.transitive_repo, vec!["barlib".to_owned()]);
        assert!(!plan.only_requested());
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

    // ---- self-edges are dropped, not reported as cycles -----------------

    #[test]
    fn aur_self_dependency_is_not_a_cycle() {
        // Regression (docs/plans/bugs.md): a pkgbase that lists its own name
        // in `depends` (a package providing/depending on itself) produced a
        // spurious `cycle: systemd-selinux → systemd-selinux` from the
        // full-graph `topo::sort`. The self-edge must be dropped before the
        // cycle check, so the package resolves normally.
        let plan = run(
            &["systemd-selinux"],
            vec![entry("systemd-selinux", &["systemd-selinux"], &[])],
            &[],
        )
        .expect("a package depending on itself must resolve, not cycle");
        assert!(plan.direct_aur.contains("systemd-selinux"));
        assert_eq!(plan.aur_strata, vec![vec!["systemd-selinux".to_owned()]]);
    }

    #[test]
    fn aur_self_makedepend_is_not_a_cycle() {
        // Same self-edge via `makedepends` (a split pkgbase whose build dep
        // names its own pkgbase). It must clear both the `topo::sort` cycle
        // check and strata — a single unblocked stratum.
        let plan = run(&["foo"], vec![entry("foo", &[], &["foo"])], &[])
            .expect("a pkgbase makedepending on itself must resolve, not cycle");
        assert_eq!(plan.aur_strata, vec![vec!["foo".to_owned()]]);
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
        let by = Lookup::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.sync_versions.insert("rust".into(), "1.80.0-1".into());
        pac.sync_providers
            .insert("cargo".into(), vec!["rust".into()]);
        pac.sync_versions.insert("pacman".into(), "6.1.0-1".into());
        pac.sync_providers
            .insert("libalpm.so".into(), vec!["pacman".into()]);
        let cfg = default_config();
        let plan = resolve(&cfg, &idx, &by, &pac, &["paru".to_owned()]).unwrap();

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
        let by = Lookup::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.installed.insert("rust".into(), "1.80.0-1".into());
        pac.installed_providers
            .insert("cargo".into(), vec!["rust".into()]);
        // rust is also in sync, but installed wins.
        pac.sync_versions.insert("rust".into(), "1.80.0-1".into());
        pac.sync_providers
            .insert("cargo".into(), vec!["rust".into()]);
        let cfg = default_config();
        let plan = resolve(&cfg, &idx, &by, &pac, &["paru".to_owned()]).unwrap();

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
        let by = Lookup::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.sync_versions.insert("rust".into(), "1.80.0-1".into());
        pac.sync_providers
            .insert("cargo".into(), vec!["rust".into()]);
        let cfg = default_config();
        let plan = resolve(
            &cfg,
            &idx,
            &by,
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
        let by = Lookup::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.installed
            .insert("brave-bin".into(), "1:1.90.121-1".into());
        let cfg = default_config();
        let plan = resolve(&cfg, &idx, &by, &pac, &["brave-bin".to_owned()]).unwrap();
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
        let by = Lookup::build(&idx);
        let mut pac = PacmanIndex::default();
        pac.installed.insert("helper".into(), "1.0-1".into());
        let cfg = default_config();
        let plan = resolve(&cfg, &idx, &by, &pac, &["client".to_owned()]).unwrap();
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
