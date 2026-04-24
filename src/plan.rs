//! Render the computed phases to stdout.

use std::collections::{HashMap, HashSet};

use petgraph::graph::NodeIndex;

use crate::graph::Dag;
use crate::metadata::{Metadata, Package};

/// Print the plan: header, then one block per phase listing the packages
/// that compile in parallel.
///
/// When `workspace_only` is true, only workspace members are shown in the
/// output. The full dependency graph is still used for phase computation —
/// this only filters what gets printed. External deps are summarized with
/// a count per phase.
pub fn print(meta: &Metadata, dag: &Dag, phases: &[Vec<NodeIndex>], workspace_only: bool) {
    let by_id: HashMap<&str, &Package> =
        meta.packages.iter().map(|p| (p.id.as_str(), p)).collect();

    let ws_members: HashSet<&str> = meta.workspace_members.iter().map(|s| s.as_str()).collect();

    let total_pkgs: usize = phases.iter().map(|p| p.len()).sum();
    let ws_count: usize = if workspace_only {
        phases.iter().flat_map(|p| p.iter()).filter(|&&idx| ws_members.contains(dag.id_of(idx))).count()
    } else {
        total_pkgs
    };

    println!("parallel-rustc plan");
    if !meta.workspace_root.is_empty() {
        println!("workspace: {}", meta.workspace_root);
    }
    if workspace_only {
        println!("packages: {} (workspace)   total: {} (with deps)   phases: {}", ws_count, total_pkgs, phases.len());
    } else {
        println!("packages: {}   phases: {}", total_pkgs, phases.len());
    }
    println!();

    for (i, phase) in phases.iter().enumerate() {
        let mut ws_pkgs: Vec<(&str, &str)> = Vec::new();
        let mut ext_count = 0usize;

        for &idx in phase {
            let id = dag.id_of(idx);
            if workspace_only && !ws_members.contains(id) {
                ext_count += 1;
                continue;
            }
            if let Some(pkg) = by_id.get(id) {
                ws_pkgs.push((pkg.name.as_str(), pkg.version.as_str()));
            } else {
                ws_pkgs.push((id, ""));
            }
        }

        if workspace_only && ws_pkgs.is_empty() {
            // Skip phases with only external deps when filtering
            if ext_count > 0 {
                println!("phase {} ({} external deps, skipped)", i, ext_count);
            }
            continue;
        }

        let label = if workspace_only && ext_count > 0 {
            format!("phase {} ({} workspace + {} external, parallel):", i, ws_pkgs.len(), ext_count)
        } else {
            format!("phase {} ({} packages, parallel):", i, phase.len())
        };
        println!("{}", label);

        for (name, version) in &ws_pkgs {
            if version.is_empty() {
                println!("  - {}", name);
            } else {
                println!("  - {} v{}", name, version);
            }
        }
    }
}
