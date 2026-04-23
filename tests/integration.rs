//! Integration tests — each test creates a temporary workspace with a specific
//! dependency topology, runs `cargo metadata`, then verifies phase assignment.

use std::fs;
use std::path::Path;
use std::process::Command;

use parallel_rustc::graph::build_phases;
use parallel_rustc::metadata::Metadata;

struct TestCrate { name: &'static str, deps: Vec<&'static str>, is_bin: bool }

impl TestCrate {
    fn lib(name: &'static str, deps: Vec<&'static str>) -> Self { Self { name, deps, is_bin: false } }
    fn bin(name: &'static str, deps: Vec<&'static str>) -> Self { Self { name, deps, is_bin: true } }
}

fn create_workspace(dir: &Path, crates: &[TestCrate]) {
    let members: Vec<String> = crates.iter().map(|c| format!("    \"{}\"", c.name)).collect();
    fs::write(dir.join("Cargo.toml"), format!("[workspace]\nmembers = [\n{}\n]\nresolver = \"2\"\n", members.join(",\n"))).unwrap();
    for c in crates {
        let d = dir.join(c.name);
        fs::create_dir_all(d.join("src")).unwrap();
        let mut toml = format!("[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n", c.name);
        if !c.deps.is_empty() {
            toml.push_str("\n[dependencies]\n");
            for dep in &c.deps { toml.push_str(&format!("{} = {{ path = \"../{}\" }}\n", dep, dep)); }
        }
        fs::write(d.join("Cargo.toml"), &toml).unwrap();
        let (src, fname) = if c.is_bin {
            (format!("fn main() {{ println!(\"{}\") }}\n", c.name), "main.rs")
        } else {
            (format!("pub fn id() -> &'static str {{ \"{}\" }}\n", c.name), "lib.rs")
        };
        fs::write(d.join("src").join(fname), src).unwrap();
    }
}

fn run_plan(workspace_dir: &Path) -> Vec<Vec<String>> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(workspace_dir.join("Cargo.toml"))
        .output().expect("cargo metadata failed");
    assert!(out.status.success(), "cargo metadata: {}", String::from_utf8_lossy(&out.stderr));
    let meta: Metadata = serde_json::from_slice(&out.stdout).unwrap();
    build_phases(&meta).iter().map(|phase| {
        let mut names: Vec<String> = phase.iter().map(|p| p.name.clone()).collect();
        names.sort();
        names
    }).collect()
}

fn assert_in_phase(phases: &[Vec<String>], name: &str, phase: usize) {
    assert!(phase < phases.len(), "expected phase {phase} for '{name}' but only {} phases", phases.len());
    assert!(phases[phase].contains(&name.to_string()), "'{name}' not in phase {phase}: {:?}\nall: {:?}", phases[phase], phases);
}

// Case 1: Wide fan-out — all leaves in phase 0
#[test]
fn wide_fanout_all_leaves_phase_0() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("leaf-a", vec![]), TestCrate::lib("leaf-b", vec![]),
        TestCrate::lib("leaf-c", vec![]), TestCrate::lib("leaf-d", vec![]),
        TestCrate::lib("leaf-e", vec![]),
    ]);
    let phases = run_plan(dir.path());
    assert_eq!(phases.len(), 1, "all leaves = single phase");
    assert_eq!(phases[0].len(), 5);
}

// Case 2: Linear chain — one per phase
#[test]
fn linear_chain_fully_serial() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("chain-1", vec![]),
        TestCrate::lib("chain-2", vec!["chain-1"]),
        TestCrate::lib("chain-3", vec!["chain-2"]),
        TestCrate::lib("chain-4", vec!["chain-3"]),
    ]);
    let phases = run_plan(dir.path());
    assert_eq!(phases.len(), 4);
    assert_in_phase(&phases, "chain-1", 0);
    assert_in_phase(&phases, "chain-2", 1);
    assert_in_phase(&phases, "chain-3", 2);
    assert_in_phase(&phases, "chain-4", 3);
}

// Case 3: Diamond — fan-out then fan-in
#[test]
fn diamond_pattern() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("base", vec![]),
        TestCrate::lib("left", vec!["base"]),
        TestCrate::lib("right", vec!["base"]),
        TestCrate::lib("top", vec!["left", "right"]),
    ]);
    let phases = run_plan(dir.path());
    assert_eq!(phases.len(), 3);
    assert_in_phase(&phases, "base", 0);
    assert_in_phase(&phases, "left", 1);
    assert_in_phase(&phases, "right", 1);
    assert_in_phase(&phases, "top", 2);
}

// Case 4: Shared dependency fan-in
#[test]
fn shared_dependency_fanin() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("shared-base", vec![]),
        TestCrate::lib("user-1", vec!["shared-base"]),
        TestCrate::lib("user-2", vec!["shared-base"]),
        TestCrate::lib("user-3", vec!["shared-base"]),
    ]);
    let phases = run_plan(dir.path());
    assert_eq!(phases.len(), 2);
    assert_in_phase(&phases, "shared-base", 0);
    assert_in_phase(&phases, "user-1", 1);
    assert_in_phase(&phases, "user-2", 1);
    assert_in_phase(&phases, "user-3", 1);
}

// Case 5: Mixed topology — chain + diamond + fan-in into final app
#[test]
fn mixed_topology_final_merge() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("leaf-a", vec![]), TestCrate::lib("leaf-b", vec![]),
        TestCrate::lib("chain-1", vec![]),
        TestCrate::lib("chain-2", vec!["chain-1"]),
        TestCrate::lib("chain-3", vec!["chain-2"]),
        TestCrate::lib("dia-left", vec!["leaf-a"]),
        TestCrate::lib("dia-right", vec!["leaf-a"]),
        TestCrate::lib("dia-top", vec!["dia-left", "dia-right"]),
        TestCrate::lib("fan-1", vec!["leaf-b"]),
        TestCrate::lib("fan-2", vec!["leaf-b"]),
        TestCrate::bin("app", vec!["chain-3", "dia-top", "fan-1"]),
    ]);
    let phases = run_plan(dir.path());
    assert_in_phase(&phases, "leaf-a", 0);
    assert_in_phase(&phases, "leaf-b", 0);
    assert_in_phase(&phases, "chain-1", 0);
    assert_in_phase(&phases, "dia-left", 1);
    assert_in_phase(&phases, "dia-right", 1);
    assert_in_phase(&phases, "fan-1", 1);
    assert_in_phase(&phases, "fan-2", 1);
    assert_in_phase(&phases, "chain-2", 1);
    assert_in_phase(&phases, "dia-top", 2);
    assert_in_phase(&phases, "chain-3", 2);
    assert_in_phase(&phases, "app", 3);
}

// Case 6: Single crate (degenerate)
#[test]
fn single_crate() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[TestCrate::lib("solo", vec![])]);
    let phases = run_plan(dir.path());
    assert_eq!(phases.len(), 1);
    assert_eq!(phases[0], vec!["solo"]);
}

// Case 7: Two independent chains — interleave phases
#[test]
fn two_independent_chains() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("chain-a1", vec![]),
        TestCrate::lib("chain-a2", vec!["chain-a1"]),
        TestCrate::lib("chain-a3", vec!["chain-a2"]),
        TestCrate::lib("chain-b1", vec![]),
        TestCrate::lib("chain-b2", vec!["chain-b1"]),
    ]);
    let phases = run_plan(dir.path());
    assert_eq!(phases.len(), 3, "depth = longest chain (3)");
    assert_in_phase(&phases, "chain-a1", 0);
    assert_in_phase(&phases, "chain-b1", 0);
    assert_in_phase(&phases, "chain-a2", 1);
    assert_in_phase(&phases, "chain-b2", 1);
    assert_in_phase(&phases, "chain-a3", 2);
}
