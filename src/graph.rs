//! Build a DAG from parsed metadata and compute wavefront phases.
//!
//! Edges point from dependency -> dependent, so a topological traversal
//! yields compile order.

use std::collections::HashMap;

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;

use crate::metadata::Metadata;

/// A DAG where each node is a package id, augmented with an index-by-id map
/// so we can render output cheaply.
#[allow(dead_code)]
pub struct Dag {
    pub graph: DiGraph<String, ()>,
    pub by_id: HashMap<String, NodeIndex>,
}

impl Dag {
    pub fn id_of(&self, idx: NodeIndex) -> &str {
        &self.graph[idx]
    }
}

/// Build the dependency DAG.
pub fn build(meta: &Metadata) -> Result<Dag, String> {
    let mut graph: DiGraph<String, ()> = DiGraph::new();
    let mut by_id: HashMap<String, NodeIndex> = HashMap::new();

    // First pass: create one node per resolved package.
    for node in &meta.resolve.nodes {
        let idx = graph.add_node(node.id.clone());
        by_id.insert(node.id.clone(), idx);
    }

    // Second pass: add dep -> dependent edges.
    for node in &meta.resolve.nodes {
        let dependent = by_id[&node.id];
        for dep_id in node.compile_deps() {
            let Some(&dep_idx) = by_id.get(dep_id) else {
                // cargo should never emit a dangling id, but be defensive.
                return Err(format!(
                    "resolve node {} references unknown dependency {}",
                    node.id, dep_id
                ));
            };
            graph.add_edge(dep_idx, dependent, ());
        }
    }

    Ok(Dag { graph, by_id })
}

/// Compute wavefront phases via layered Kahn's algorithm.
///
/// Returns `Vec<Vec<NodeIndex>>` where each inner vec is one phase
/// (a set of nodes that can compile in parallel). Nodes within a phase
/// are sorted by package id for stable, diff-friendly output.
///
/// Errors if the graph contains a cycle.
pub fn phases(dag: &Dag) -> Result<Vec<Vec<NodeIndex>>, String> {
    let g = &dag.graph;
    let mut indeg: HashMap<NodeIndex, usize> = HashMap::with_capacity(g.node_count());
    for n in g.node_indices() {
        indeg.insert(n, g.neighbors_directed(n, Direction::Incoming).count());
    }

    let mut phases: Vec<Vec<NodeIndex>> = Vec::new();
    let mut ready: Vec<NodeIndex> = indeg
        .iter()
        .filter_map(|(&n, &d)| (d == 0).then_some(n))
        .collect();
    // Stable ordering by package id.
    ready.sort_by(|a, b| g[*a].cmp(&g[*b]));

    let mut emitted = 0usize;
    while !ready.is_empty() {
        phases.push(ready.clone());
        emitted += ready.len();

        let mut next: Vec<NodeIndex> = Vec::new();
        for &n in &ready {
            for m in g.neighbors_directed(n, Direction::Outgoing) {
                let d = indeg.get_mut(&m).expect("indeg entry");
                *d -= 1;
                if *d == 0 {
                    next.push(m);
                }
            }
        }
        next.sort_by(|a, b| g[*a].cmp(&g[*b]));
        ready = next;
    }

    if emitted != g.node_count() {
        return Err(format!(
            "dependency graph contains a cycle: emitted {} of {} nodes",
            emitted,
            g.node_count()
        ));
    }
    Ok(phases)
}

/// Convenience: build the graph and compute phases in one shot, returning
/// the phases as `Vec<Vec<Package>>` (package info, not node indices).
/// Used primarily by integration tests.
pub fn build_phases(meta: &Metadata) -> Vec<Vec<crate::metadata::Package>> {
    let dag = build(meta).expect("graph build failed");
    let ph = phases(&dag).expect("phase computation failed");
    ph.iter()
        .map(|phase_nodes| {
            let mut pkgs: Vec<crate::metadata::Package> = phase_nodes
                .iter()
                .map(|&idx| {
                    let id = dag.id_of(idx);
                    meta.packages
                        .iter()
                        .find(|p| p.id == id)
                        .cloned()
                        .expect("package not found for id")
                })
                .collect();
            pkgs.sort_by(|a, b| a.name.cmp(&b.name));
            pkgs
        })
        .collect()
}
