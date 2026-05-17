//! Topological sort over the AUR dep subgraph, with cycle reporting.

use crate::error::{Error, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::BuildHasher;

fn visit<S: BuildHasher>(
    node: &str,
    edges: &HashMap<String, Vec<String>, S>,
    nodes: &BTreeSet<String>,
    visited: &mut HashSet<String>,
    on_stack: &mut HashSet<String>,
    stack: &mut Vec<String>,
    order: &mut Vec<String>,
) -> Result<()> {
    if visited.contains(node) {
        return Ok(());
    }
    if !on_stack.insert(node.to_string()) {
        let mut path: Vec<String> = stack.clone();
        path.push(node.to_string());
        return Err(Error::Resolve(format!("cycle: {}", path.join(" → "))));
    }
    stack.push(node.to_string());

    if let Some(deps) = edges.get(node) {
        for d in deps {
            if nodes.contains(d) {
                visit(d, edges, nodes, visited, on_stack, stack, order)?;
            }
        }
    }

    stack.pop();
    on_stack.remove(node);
    visited.insert(node.to_string());
    order.push(node.to_string());
    Ok(())
}

/// Tarjan-style DFS yielding a build order. On cycle, returns
/// `Err(Error::Resolve(...))` with the offending path.
pub fn sort<S: BuildHasher>(
    edges: &HashMap<String, Vec<String>, S>,
    nodes: &BTreeSet<String>,
) -> Result<Vec<String>> {
    let mut order = Vec::with_capacity(nodes.len());
    let mut visited: HashSet<String> = HashSet::new();
    let mut on_stack: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = Vec::new();

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
}
