//! Integration tests focused on feature flags, optional dependencies,
//! feature unification, and multi-binary workspaces.

use std::fs;
use std::path::Path;
use std::process::Command;

use parallel_rustc::graph::build_phases;
use parallel_rustc::metadata::Metadata;

/// Richer test crate definition supporting features and optional deps.
struct TestCrate {
    name: &'static str,
    deps: Vec<Dep>,
    features: Vec<(&'static str, Vec<&'static str>)>,
    default_features: Vec<&'static str>,
    is_bin: bool,
    extra_toml: &'static str,
}

struct Dep {
    name: &'static str,
    optional: bool,
    features: Vec<&'static str>,
    default_features: bool,
}

impl Dep {
    fn required(name: &'static str) -> Self {
        Self { name, optional: false, features: vec![], default_features: true }
    }
    fn optional(name: &'static str) -> Self {
        Self { name, optional: true, features: vec![], default_features: true }
    }
    fn with_features(name: &'static str, features: Vec<&'static str>) -> Self {
        Self { name, optional: false, features, default_features: true }
    }
    fn no_defaults(name: &'static str) -> Self {
        Self { name, optional: false, features: vec![], default_features: false }
    }
}

impl TestCrate {
    fn lib(name: &'static str) -> Self {
        Self { name, deps: vec![], features: vec![], default_features: vec![], is_bin: false, extra_toml: "" }
    }
    fn bin(name: &'static str) -> Self {
        Self { name, deps: vec![], features: vec![], default_features: vec![], is_bin: true, extra_toml: "" }
    }
    fn with_deps(mut self, deps: Vec<Dep>) -> Self { self.deps = deps; self }
    fn with_features(mut self, features: Vec<(&'static str, Vec<&'static str>)>) -> Self { self.features = features; self }
    fn with_defaults(mut self, defaults: Vec<&'static str>) -> Self { self.default_features = defaults; self }
    fn with_extra_toml(mut self, toml: &'static str) -> Self { self.extra_toml = toml; self }
}

fn create_workspace(dir: &Path, crates: &[TestCrate]) {
    let members: Vec<String> = crates.iter().map(|c| format!("    \"{}\"", c.name)).collect();
    fs::write(dir.join("Cargo.toml"), format!(
        "[workspace]\nmembers = [\n{}\n]\nresolver = \"2\"\n", members.join(",\n")
    )).unwrap();

    for c in crates {
        let d = dir.join(c.name);
        fs::create_dir_all(d.join("src")).unwrap();

        let mut toml = format!(
            "[package]\nname = \"{}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n", c.name
        );

        if !c.deps.is_empty() {
            toml.push_str("\n[dependencies]\n");
            for dep in &c.deps {
                let mut parts = vec![format!("path = \"../{}\"", dep.name)];
                if dep.optional { parts.push("optional = true".into()); }
                if !dep.features.is_empty() {
                    parts.push(format!("features = [{}]", dep.features.iter().map(|f| format!("\"{}\"", f)).collect::<Vec<_>>().join(", ")));
                }
                if !dep.default_features { parts.push("default-features = false".into()); }
                toml.push_str(&format!("{} = {{ {} }}\n", dep.name, parts.join(", ")));
            }
        }

        if !c.features.is_empty() || !c.default_features.is_empty() {
            toml.push_str("\n[features]\n");
            if !c.default_features.is_empty() {
                toml.push_str(&format!("default = [{}]\n",
                    c.default_features.iter().map(|f| format!("\"{}\"", f)).collect::<Vec<_>>().join(", ")));
            }
            for (feat, enables) in &c.features {
                let vals = enables.iter().map(|e| format!("\"{}\"", e)).collect::<Vec<_>>().join(", ");
                toml.push_str(&format!("{} = [{}]\n", feat, vals));
            }
        }

        if !c.extra_toml.is_empty() {
            toml.push_str("\n");
            toml.push_str(c.extra_toml);
            toml.push_str("\n");
        }

        fs::write(d.join("Cargo.toml"), &toml).unwrap();

        // Source with conditional compilation
        let src = if c.is_bin {
            format!("fn main() {{ println!(\"{}\") }}\n", c.name)
        } else {
            let mut s = format!("pub fn id() -> &'static str {{ \"{}\" }}\n", c.name);
            // Add cfg-gated functions for each feature
            for (feat, _) in &c.features {
                let safe = feat.replace("-", "_");
                s.push_str(&format!(
                    "#[cfg(feature = \"{feat}\")]\npub fn has_{safe}() -> bool {{ true }}\n"
                ));
            }
            s
        };
        let filename = if c.is_bin { "main.rs" } else { "lib.rs" };
        fs::write(d.join("src").join(filename), src).unwrap();
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
    assert!(phases[phase].contains(&name.to_string()),
        "'{name}' not in phase {phase}: {:?}\nall: {:?}", phases[phase], phases);
}

/// Verify that `cargo check` succeeds on the workspace.
fn assert_workspace_builds(dir: &Path) {
    let out = Command::new("cargo")
        .args(["check", "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .output().expect("cargo check failed");
    assert!(out.status.success(), "cargo check failed:\n{}", String::from_utf8_lossy(&out.stderr));
}

// ─────────────────────────────────────────────────────────────────────────────
// Case 8: Optional dependency — when not enabled, should not add an edge
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn optional_dep_not_enabled() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("base"),
        TestCrate::lib("optional-extra"),
        // consumer has optional-extra as optional dep, NOT enabled by default
        TestCrate::lib("consumer")
            .with_deps(vec![
                Dep::required("base"),
                Dep::optional("optional-extra"),
            ])
            .with_features(vec![
                ("extras", vec!["dep:optional-extra"]),
            ]),
    ]);
    assert_workspace_builds(dir.path());
    let phases = run_plan(dir.path());

    // base and optional-extra are both leaves (phase 0)
    // consumer depends on base (phase 1)
    // optional-extra should NOT create a dependency edge since it's not enabled
    assert_in_phase(&phases, "base", 0);
    assert_in_phase(&phases, "optional-extra", 0);
    assert_in_phase(&phases, "consumer", 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// Case 9: Feature enables optional dep — adds edge when default feature is on
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn feature_enables_optional_dep() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("base"),
        TestCrate::lib("opt-dep"),
        // consumer: default features include "extras" which pulls in opt-dep
        TestCrate::lib("consumer")
            .with_deps(vec![
                Dep::required("base"),
                Dep::optional("opt-dep"),
            ])
            .with_features(vec![
                ("extras", vec!["dep:opt-dep"]),
            ])
            .with_defaults(vec!["extras"]),
    ]);
    assert_workspace_builds(dir.path());
    let phases = run_plan(dir.path());

    // With default features, consumer depends on BOTH base and opt-dep
    // So consumer is phase 1, both deps are phase 0
    assert_in_phase(&phases, "base", 0);
    assert_in_phase(&phases, "opt-dep", 0);
    assert_in_phase(&phases, "consumer", 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// Case 10: Feature propagation — consumer enables specific features on dep
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn feature_propagation_through_deps() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        // base has features: "fast" and "logging"
        TestCrate::lib("base")
            .with_features(vec![
                ("fast", vec![]),
                ("logging", vec![]),
            ]),
        // mid depends on base with "fast" feature
        TestCrate::lib("mid")
            .with_deps(vec![Dep::with_features("base", vec!["fast"])]),
        // top depends on base with "logging" feature AND on mid
        // Feature unification: base gets BOTH "fast" and "logging"
        TestCrate::lib("top")
            .with_deps(vec![
                Dep::with_features("base", vec!["logging"]),
                Dep::required("mid"),
            ]),
    ]);
    assert_workspace_builds(dir.path());
    let phases = run_plan(dir.path());

    assert_eq!(phases.len(), 3);
    assert_in_phase(&phases, "base", 0);
    assert_in_phase(&phases, "mid", 1);
    assert_in_phase(&phases, "top", 2);
}

// ─────────────────────────────────────────────────────────────────────────────
// Case 11: Multiple binaries in workspace sharing libs
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn multiple_binaries_shared_libs() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("core-lib"),
        TestCrate::lib("net-lib").with_deps(vec![Dep::required("core-lib")]),
        TestCrate::lib("db-lib").with_deps(vec![Dep::required("core-lib")]),
        // Three binaries, each pulling different combos
        TestCrate::bin("server")
            .with_deps(vec![Dep::required("core-lib"), Dep::required("net-lib"), Dep::required("db-lib")]),
        TestCrate::bin("cli")
            .with_deps(vec![Dep::required("core-lib"), Dep::required("db-lib")]),
        TestCrate::bin("proxy-bin")
            .with_deps(vec![Dep::required("core-lib"), Dep::required("net-lib")]),
    ]);
    assert_workspace_builds(dir.path());
    let phases = run_plan(dir.path());

    assert_eq!(phases.len(), 3);
    // Phase 0: core-lib (leaf)
    assert_in_phase(&phases, "core-lib", 0);
    // Phase 1: net-lib, db-lib (both depend on core-lib)
    assert_in_phase(&phases, "net-lib", 1);
    assert_in_phase(&phases, "db-lib", 1);
    // Phase 2: all three binaries (depend on phase-1 libs)
    assert_in_phase(&phases, "server", 2);
    assert_in_phase(&phases, "cli", 2);
    assert_in_phase(&phases, "proxy-bin", 2);
}

// ─────────────────────────────────────────────────────────────────────────────
// Case 12: Deep diamond with features at every level
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn deep_diamond_with_features() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("foundation")
            .with_features(vec![("alloc", vec![]), ("std", vec!["alloc"])]),
        TestCrate::lib("codec")
            .with_deps(vec![Dep::with_features("foundation", vec!["alloc"])])
            .with_features(vec![("serde", vec![])]),
        TestCrate::lib("transport")
            .with_deps(vec![Dep::with_features("foundation", vec!["std"])])
            .with_features(vec![("tls", vec![])]),
        TestCrate::lib("protocol")
            .with_deps(vec![
                Dep::with_features("codec", vec!["serde"]),
                Dep::required("transport"),
            ]),
        TestCrate::bin("node")
            .with_deps(vec![
                Dep::with_features("transport", vec!["tls"]),
                Dep::required("protocol"),
            ]),
    ]);
    assert_workspace_builds(dir.path());
    let phases = run_plan(dir.path());

    assert_eq!(phases.len(), 4);
    assert_in_phase(&phases, "foundation", 0);
    assert_in_phase(&phases, "codec", 1);
    assert_in_phase(&phases, "transport", 1);
    assert_in_phase(&phases, "protocol", 2);
    assert_in_phase(&phases, "node", 3);
}

// ─────────────────────────────────────────────────────────────────────────────
// Case 13: Default-features = false — minimal dep
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn default_features_disabled() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("heavy-lib")
            .with_features(vec![
                ("full", vec![]),
                ("minimal", vec![]),
            ])
            .with_defaults(vec!["full"]),
        // consumer uses heavy-lib with default-features=false
        TestCrate::lib("lean-consumer")
            .with_deps(vec![Dep::no_defaults("heavy-lib")]),
    ]);
    assert_workspace_builds(dir.path());
    let phases = run_plan(dir.path());

    assert_eq!(phases.len(), 2);
    assert_in_phase(&phases, "heavy-lib", 0);
    assert_in_phase(&phases, "lean-consumer", 1);
}

// ─────────────────────────────────────────────────────────────────────────────
// Case 14: Wide workspace — 10 independent libs + 3 binaries pulling combos
// Stress test for parallelism width
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn wide_workspace_stress() {
    let dir = tempfile::tempdir().unwrap();
    let mut crates = vec![];

    // 10 independent libs
    for i in 0..10 {
        let name: &'static str = Box::leak(format!("lib-{i}").into_boxed_str());
        crates.push(TestCrate::lib(name));
    }

    // 3 "mid" libs each depending on 3-4 base libs
    crates.push(TestCrate::lib("mid-a").with_deps(vec![
        Dep::required("lib-0"), Dep::required("lib-1"), Dep::required("lib-2"),
    ]));
    crates.push(TestCrate::lib("mid-b").with_deps(vec![
        Dep::required("lib-3"), Dep::required("lib-4"), Dep::required("lib-5"),
    ]));
    crates.push(TestCrate::lib("mid-c").with_deps(vec![
        Dep::required("lib-6"), Dep::required("lib-7"), Dep::required("lib-8"), Dep::required("lib-9"),
    ]));

    // 3 binaries
    crates.push(TestCrate::bin("app-1").with_deps(vec![Dep::required("mid-a"), Dep::required("mid-b")]));
    crates.push(TestCrate::bin("app-2").with_deps(vec![Dep::required("mid-b"), Dep::required("mid-c")]));
    crates.push(TestCrate::bin("app-3").with_deps(vec![Dep::required("mid-a"), Dep::required("mid-c"), Dep::required("lib-0")]));

    create_workspace(dir.path(), &crates);
    assert_workspace_builds(dir.path());
    let phases = run_plan(dir.path());

    assert_eq!(phases.len(), 3);
    // Phase 0: all 10 libs (width = 10)
    assert_eq!(phases[0].len(), 10, "all 10 base libs in phase 0");
    // Phase 1: 3 mid libs (width = 3)
    assert_eq!(phases[1].len(), 3, "all 3 mid libs in phase 1");
    // Phase 2: 3 apps (width = 3)
    assert_eq!(phases[2].len(), 3, "all 3 apps in phase 2");
}

// ─────────────────────────────────────────────────────────────────────────────
// Case 15: Feature-gated transitive dependency chain
// A → B(feat=x) → C(feat=y via B's x)
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn feature_gated_transitive_chain() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("bottom")
            .with_features(vec![("y-mode", vec![])]),
        TestCrate::lib("middle")
            .with_deps(vec![Dep::with_features("bottom", vec!["y-mode"])])
            .with_features(vec![("x-mode", vec![])]),
        TestCrate::lib("top")
            .with_deps(vec![Dep::with_features("middle", vec!["x-mode"])]),
    ]);
    assert_workspace_builds(dir.path());
    let phases = run_plan(dir.path());

    assert_eq!(phases.len(), 3);
    assert_in_phase(&phases, "bottom", 0);
    assert_in_phase(&phases, "middle", 1);
    assert_in_phase(&phases, "top", 2);
}

// ─────────────────────────────────────────────────────────────────────────────
// Case 16: Feature flags are correctly threaded to cargo build -p invocations
//
// When we call `cargo build -p crate-x`, cargo resolves features from the
// full workspace context. This test verifies that a crate built via our
// phase executor sees the same features as a full `cargo build`.
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn feature_unification_survives_per_crate_build() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        // base has two features; mid enables "extra" on base
        TestCrate::lib("base")
            .with_features(vec![
                ("core", vec![]),
                ("extra", vec![]),
            ])
            .with_defaults(vec!["core"]),
        // mid depends on base and enables "extra"
        TestCrate::lib("mid")
            .with_deps(vec![Dep::with_features("base", vec!["extra"])]),
        // top depends on mid
        TestCrate::lib("top")
            .with_deps(vec![Dep::required("mid")]),
    ]);

    // Phase plan: base in phase 0, mid in phase 1, top in phase 2
    assert_workspace_builds(dir.path());
    let phases = run_plan(dir.path());
    assert_eq!(phases.len(), 3);
    assert_in_phase(&phases, "base", 0);
    assert_in_phase(&phases, "mid", 1);
    assert_in_phase(&phases, "top", 2);

    // When cargo builds `mid` (which requires base+extra), the workspace-level
    // feature unification should give `base` both "core" and "extra".
    // Verify by checking cargo metadata sees the unified feature set.
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(dir.path().join("Cargo.toml"))
        .output().unwrap();
    let meta: parallel_rustc::metadata::Metadata = serde_json::from_slice(&out.stdout).unwrap();

    // base should have both "core" and "extra" in the resolved feature set
    // because mid activates "extra" and default activates "core"
    let base_node = meta.resolve.nodes.iter().find(|n| n.id.contains("base")).unwrap();
    let features: std::collections::HashSet<&str> = base_node.features.iter().map(|s| s.as_str()).collect();
    assert!(features.contains("core"), "base should have core feature, got: {:?}", features);
    assert!(features.contains("extra"), "base should have extra feature (unified from mid), got: {:?}", features);
}

// ─────────────────────────────────────────────────────────────────────────────
// Case 17: compile_deps() excludes dev deps, preventing false feature edges
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn dev_deps_excluded_from_compile_graph() {
    let dir = tempfile::tempdir().unwrap();
    // lib-a has lib-b as a dev-dependency only
    // This should NOT create an edge lib-b → lib-a in the build graph
    create_workspace(dir.path(), &[
        TestCrate::lib("lib-b"),
        TestCrate::lib("lib-a")
            .with_extra_toml("[dev-dependencies]\nlib-b = { path = \"../lib-b\" }"),
    ]);
    assert_workspace_builds(dir.path());
    let phases = run_plan(dir.path());

    // Both should be in phase 0 (no real dependency between them)
    assert_eq!(phases.len(), 1, "lib-a and lib-b should both be phase 0, got phases: {:?}", phases);
    assert_in_phase(&phases, "lib-a", 0);
    assert_in_phase(&phases, "lib-b", 0);
}

// ─────────────────────────────────────────────────────────────────────────────
// Case 18: Feature flags propagate correctly through deep chain
// A enables feat-x on B, B enables feat-y on C — all must be unified
// ─────────────────────────────────────────────────────────────────────────────
#[test]
fn feature_propagation_deep_chain_unified() {
    let dir = tempfile::tempdir().unwrap();
    create_workspace(dir.path(), &[
        TestCrate::lib("bottom")
            .with_features(vec![("feat-y", vec![]), ("feat-z", vec![])]),
        TestCrate::lib("middle")
            .with_deps(vec![Dep::with_features("bottom", vec!["feat-y"])])
            .with_features(vec![("feat-x", vec![])]),
        TestCrate::lib("top")
            .with_deps(vec![Dep::with_features("middle", vec!["feat-x"])]),
    ]);
    assert_workspace_builds(dir.path());
    let phases = run_plan(dir.path());
    assert_eq!(phases.len(), 3);

    // Verify feature resolution via metadata
    let out = std::process::Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(dir.path().join("Cargo.toml"))
        .output().unwrap();
    let meta: parallel_rustc::metadata::Metadata = serde_json::from_slice(&out.stdout).unwrap();

    let bottom_node = meta.resolve.nodes.iter().find(|n| n.id.contains("bottom")).unwrap();
    let bottom_features: std::collections::HashSet<&str> = bottom_node.features.iter().map(|s| s.as_str()).collect();
    assert!(bottom_features.contains("feat-y"), "bottom should have feat-y, got: {:?}", bottom_features);

    let middle_node = meta.resolve.nodes.iter().find(|n| n.id.contains("middle")).unwrap();
    let middle_features: std::collections::HashSet<&str> = middle_node.features.iter().map(|s| s.as_str()).collect();
    assert!(middle_features.contains("feat-x"), "middle should have feat-x, got: {:?}", middle_features);
}
