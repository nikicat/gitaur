//! Topological sort + Kahn-stratification over the AUR dep subgraph,
//! with cycle reporting.
//!
//! * [`sort`] yields a flat build order (used for cycle detection over the
//!   full dependency graph, including runtime `depends`). Edge values are
//!   raw dep strings — pre-resolution they're a mix of pkgnames, virtuals,
//!   and pkgbases — so the function works in terms of `String` deps with
//!   `K: Borrow<str>` for in-graph lookups.
//! * [`strata`] groups nodes into independent layers, used for scheduling
//!   build/install rounds: every pkg in stratum N has all its edges to nodes
//!   in strata `< N` only, so the stratum can be built in parallel and then
//!   `pacman -U`'d before stratum N+1 begins. Edge values are `K` because
//!   the graph is fully resolved by then.

use crate::error::{Error, Result};
use std::borrow::Borrow;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::hash::{BuildHasher, Hash};

/// Recursive DFS helper for [`sort`]. Walks via `nodes.get(d.as_str())` so
/// in-graph deps resolve by `Borrow<str>` to the typed key.
fn visit<K, S>(
    node: &K,
    edges: &HashMap<K, Vec<String>, S>,
    nodes: &BTreeSet<K>,
    visited: &mut HashSet<K>,
    on_stack: &mut HashSet<K>,
    stack: &mut Vec<K>,
    order: &mut Vec<K>,
) -> Result<()>
where
    K: Eq + Hash + Ord + Clone + Borrow<str> + fmt::Display,
    S: BuildHasher,
{
    // Use `HashSet::contains::<K>` (turbofish) on the K-keyed methods so the
    // compiler picks `Borrow<K>` (the blanket `T: Borrow<T>` impl) instead of
    // `Borrow<str>` — both are in scope on K and would otherwise be
    // ambiguous, since this fn requires both.
    if HashSet::<K, _>::contains::<K>(visited, node) {
        return Ok(());
    }
    if !on_stack.insert(node.clone()) {
        let mut path: Vec<String> = stack.iter().map(ToString::to_string).collect();
        path.push(node.to_string());
        return Err(Error::Resolve(format!("cycle: {}", path.join(" → "))));
    }
    stack.push(node.clone());

    if let Some(deps) = HashMap::<K, Vec<String>, S>::get::<K>(edges, node) {
        for d in deps {
            if let Some(d_k) = nodes.get(d.as_str()) {
                let d_k = d_k.clone();
                visit(&d_k, edges, nodes, visited, on_stack, stack, order)?;
            }
        }
    }

    stack.pop();
    HashSet::<K, _>::remove::<K>(on_stack, node);
    visited.insert(node.clone());
    order.push(node.clone());
    Ok(())
}

/// Tarjan-style DFS yielding a build order. On cycle, returns
/// `Err(Error::Resolve(...))` with the offending path.
///
/// `K` is the node identity (e.g. `PkgBase`); edges values are raw `String`
/// dep names, looked up against `nodes` via `K: Borrow<str>`.
pub fn sort<K, S>(edges: &HashMap<K, Vec<String>, S>, nodes: &BTreeSet<K>) -> Result<Vec<K>>
where
    K: Eq + Hash + Ord + Clone + Borrow<str> + fmt::Display,
    S: BuildHasher,
{
    let mut order = Vec::with_capacity(nodes.len());
    let mut visited: HashSet<K> = HashSet::new();
    let mut on_stack: HashSet<K> = HashSet::new();
    let mut stack: Vec<K> = Vec::new();

    for n in nodes {
        visit(
            n,
            edges,
            nodes,
            &mut visited,
            &mut on_stack,
            &mut stack,
            &mut order,
        )?;
    }
    Ok(order)
}

/// Group nodes into Kahn-order strata.
///
/// Stratum 0 has nodes with no edges to other in-graph nodes; stratum N+1
/// has nodes whose remaining in-graph edges all point to strata `≤ N`.
/// Returns an error containing the offending node set if `edges` contains
/// a cycle restricted to `nodes`.
///
/// The `edges` map is interpreted the same way as in [`sort`]: `edges[a]`
/// lists nodes that must be built **before** `a`. Edges pointing to names
/// outside `nodes` (e.g. repo-resolved deps) are silently ignored.
///
/// Both edge values and node keys are typed `K` — strata runs on a fully
/// resolved graph where deps have already been narrowed to in-graph
/// pkgbases.
pub fn strata<K, S>(edges: &HashMap<K, Vec<K>, S>, nodes: &BTreeSet<K>) -> Result<Vec<Vec<K>>>
where
    K: Eq + Hash + Ord + Clone + fmt::Debug,
    S: BuildHasher,
{
    let mut remaining: HashMap<K, usize> = nodes
        .iter()
        .map(|n| {
            let count = edges
                .get(n)
                .map_or(0, |deps| deps.iter().filter(|d| nodes.contains(*d)).count());
            (n.clone(), count)
        })
        .collect();

    let mut out: Vec<Vec<K>> = Vec::new();
    while !remaining.is_empty() {
        let mut ready: Vec<K> = remaining
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(n, _)| n.clone())
            .collect();
        if ready.is_empty() {
            let mut leftover: Vec<K> = remaining.keys().cloned().collect();
            leftover.sort();
            return Err(Error::Resolve(format!("cycle among: {leftover:?}")));
        }
        ready.sort();
        for r in &ready {
            remaining.remove(r);
        }
        // Decrement in-degree of pkgs that named anything in this stratum.
        for (n, deps) in edges {
            if let Some(deg) = remaining.get_mut(n) {
                let removed = deps.iter().filter(|d| ready.contains(d)).count();
                *deg = deg.saturating_sub(removed);
            }
        }
        out.push(ready);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|x| (*x).into()).collect()
    }

    #[test]
    fn linear_chain() {
        let nodes: BTreeSet<String> = ["a", "b", "c"].iter().map(|x| (*x).into()).collect();
        let mut e = HashMap::new();
        e.insert("a".into(), s(&["b"]));
        e.insert("b".into(), s(&["c"]));
        e.insert("c".into(), s(&[]));
        let order = sort(&e, &nodes).unwrap();
        assert_eq!(order, s(&["c", "b", "a"]));
    }

    #[test]
    fn detects_cycle() {
        let nodes: BTreeSet<String> = ["a", "b"].iter().map(|x| (*x).into()).collect();
        let mut e = HashMap::new();
        e.insert("a".into(), s(&["b"]));
        e.insert("b".into(), s(&["a"]));
        let err = sort(&e, &nodes).unwrap_err();
        assert!(matches!(err, Error::Resolve(_)));
    }

    #[test]
    fn ignores_non_aur_edges() {
        // c is not in the node set (it's a repo dep), so it should be skipped.
        let nodes: BTreeSet<String> = ["a", "b"].iter().map(|x| (*x).into()).collect();
        let mut e = HashMap::new();
        e.insert("a".into(), s(&["b", "c"]));
        e.insert("b".into(), s(&["c"]));
        let order = sort(&e, &nodes).unwrap();
        assert_eq!(order, s(&["b", "a"]));
    }

    // ---- strata --------------------------------------------------------

    fn n(strs: &[&str]) -> BTreeSet<String> {
        strs.iter().map(|x| (*x).into()).collect()
    }

    #[test]
    fn strata_empty_input() {
        let empty: HashMap<String, Vec<String>> = HashMap::new();
        let out = strata(&empty, &BTreeSet::<String>::new()).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn strata_no_edges_all_in_one() {
        let nodes = n(&["a", "b", "c"]);
        let out = strata(&HashMap::new(), &nodes).unwrap();
        assert_eq!(out, vec![s(&["a", "b", "c"])]);
    }

    #[test]
    fn strata_linear_chain() {
        // a → b → c (a needs b, b needs c). c builds first, then b, then a.
        let nodes = n(&["a", "b", "c"]);
        let mut e = HashMap::new();
        e.insert("a".into(), s(&["b"]));
        e.insert("b".into(), s(&["c"]));
        let out = strata(&e, &nodes).unwrap();
        assert_eq!(out, vec![s(&["c"]), s(&["b"]), s(&["a"])]);
    }

    #[test]
    fn strata_diamond() {
        // d depends on b and c; b and c both depend on a. Layers: [a], [b,c], [d].
        let nodes = n(&["a", "b", "c", "d"]);
        let mut e = HashMap::new();
        e.insert("d".into(), s(&["b", "c"]));
        e.insert("b".into(), s(&["a"]));
        e.insert("c".into(), s(&["a"]));
        let out = strata(&e, &nodes).unwrap();
        assert_eq!(out, vec![s(&["a"]), s(&["b", "c"]), s(&["d"])]);
    }

    #[test]
    fn strata_independent_components_share_a_stratum() {
        // Two disconnected chains: a→b and c→d. Layers: [b,d], [a,c].
        let nodes = n(&["a", "b", "c", "d"]);
        let mut e = HashMap::new();
        e.insert("a".into(), s(&["b"]));
        e.insert("c".into(), s(&["d"]));
        let out = strata(&e, &nodes).unwrap();
        assert_eq!(out, vec![s(&["b", "d"]), s(&["a", "c"])]);
    }

    #[test]
    fn strata_ignores_non_node_edges() {
        // a depends on b (in-graph) AND x (repo dep, not in nodes).
        let nodes = n(&["a", "b"]);
        let mut e = HashMap::new();
        e.insert("a".into(), s(&["b", "x"]));
        let out = strata(&e, &nodes).unwrap();
        assert_eq!(out, vec![s(&["b"]), s(&["a"])]);
    }

    #[test]
    fn strata_detects_cycle() {
        let nodes = n(&["a", "b"]);
        let mut e = HashMap::new();
        e.insert("a".into(), s(&["b"]));
        e.insert("b".into(), s(&["a"]));
        let err = strata(&e, &nodes).unwrap_err();
        match err {
            Error::Resolve(msg) => {
                assert!(msg.contains('a') && msg.contains('b'), "got: {msg}");
            }
            other => panic!("expected Resolve, got {other:?}"),
        }
    }

    #[test]
    fn strata_self_loop_is_cycle() {
        let nodes = n(&["a"]);
        let mut e = HashMap::new();
        e.insert("a".into(), s(&["a"]));
        assert!(strata(&e, &nodes).is_err());
    }

    #[test]
    fn strata_dedups_repeated_edges() {
        // edges list b twice for a — in-degree should still be 1, single
        // stratum split.
        let nodes = n(&["a", "b"]);
        let mut e = HashMap::new();
        e.insert("a".into(), s(&["b", "b"]));
        let out = strata(&e, &nodes).unwrap();
        // After b's stratum, a's degree should hit 0 in one tick. With our
        // current impl, double-counting b inflates in-degree to 2 and we'd
        // decrement by 2 when b lands — net same result. Validate this.
        assert_eq!(out, vec![s(&["b"]), s(&["a"])]);
    }

    #[test]
    fn strata_multi_layer_realistic_aur_graph() {
        // Simulating: pkgA makedepends pkgB; pkgB makedepends pkgC and pkgD;
        // pkgD makedepends pkgC. Expected layers: [C], [B,D would be wrong —
        // D depends on C, so D after C. B depends on C and D, so B after D].
        // Result: [C], [D], [B], [A].
        let nodes = n(&["a", "b", "c", "d"]);
        let mut e = HashMap::new();
        e.insert("a".into(), s(&["b"]));
        e.insert("b".into(), s(&["c", "d"]));
        e.insert("d".into(), s(&["c"]));
        let out = strata(&e, &nodes).unwrap();
        assert_eq!(out, vec![s(&["c"]), s(&["d"]), s(&["b"]), s(&["a"])]);
    }

    #[test]
    fn strata_round_trips_with_sort_on_acyclic_graph() {
        // Stratification preserves the partial order: every node in stratum
        // N must precede every node in stratum N+M (for M>0) in `sort`'s
        // output. Property-style spot check on the diamond.
        let nodes = n(&["a", "b", "c", "d"]);
        let mut e = HashMap::new();
        e.insert("d".into(), s(&["b", "c"]));
        e.insert("b".into(), s(&["a"]));
        e.insert("c".into(), s(&["a"]));
        let layers = strata(&e, &nodes).unwrap();
        let flat = sort(&e, &nodes).unwrap();
        for (i, stratum) in layers.iter().enumerate() {
            for name in stratum {
                let flat_pos = flat.iter().position(|x| x == name).unwrap();
                // Every node in later strata must appear later in `flat`.
                for later in layers.iter().skip(i + 1).flatten() {
                    let later_pos = flat.iter().position(|x| x == later).unwrap();
                    assert!(
                        flat_pos < later_pos,
                        "{name} (stratum {i}) should precede {later} in sort()",
                    );
                }
            }
        }
    }
}
