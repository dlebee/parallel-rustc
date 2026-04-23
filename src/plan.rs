//! Render the computed phases to stdout.

use std::collections::HashMap;

use petgraph::graph::NodeIndex;

use crate::graph::Dag;
use crate::metadata::{Metadata, Package};

/// Print the plan: header, then one block per phase listing the packages
/// that compile in parallel.
pub fn print(meta: &Metadata, dag: &Dag, phases: &[Vec<NodeIndex>]) {
    let by_id: HashMap<&str, &Package> =
        meta.packages.iter().map(|p| (p.id.as_str(), p)).collect();

    let total_pkgs: usize = phases.iter().map(|p| p.len()).sum();
    println!("parallel-rustc plan");
    if !meta.workspace_root.is_empty() {
        println!("workspace: {}", meta.workspace_root);
    }
    println!("packages: {}   phases: {}", total_pkgs, phases.len());
    println!();

    for (i, phase) in phases.iter().enumerate() {
        println!("phase {} ({} packages, parallel):", i, phase.len());
        for &idx in phase {
            let id = dag.id_of(idx);
            match by_id.get(id) {
                Some(pkg) => println!("  - {} v{}", pkg.name, pkg.version),
                None => println!("  - {}", id),
            }
        }
    }
}
