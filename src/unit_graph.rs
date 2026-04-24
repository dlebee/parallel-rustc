//! Cargo `--unit-graph` loader and phase planner (v0.3.0).
//!
//! Unlike `metadata.rs`/`graph.rs`, which infer a per-package compile
//! DAG from `cargo metadata`, this module consumes Cargo's nightly
//! `--unit-graph -Z unstable-options` output: the *actual* list of
//! compilation units cargo would run, with their dependency edges
//! already resolved (profile, features, build-scripts, etc).
//!
//! This is the data we want to drive parallel compilation from, because:
//!
//!   * It distinguishes `build` / `run-custom-build` / `check` modes,
//!     so we know which units are real rustc invocations and which
//!     are build-script executions.
//!   * Each unit already has its resolved feature set.
//!   * Dependencies are indices into the same units array — the phase
//!     computation is a plain Kahn's algorithm on integers, no string
//!     matching / pkg_id parsing / feature guessing.
//!
//! Requires a nightly toolchain. We always invoke `cargo +nightly ...`
//! regardless of the caller's default toolchain.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

/// One compilation unit as produced by cargo.
#[derive(Debug, Clone)]
pub struct UnitGraphUnit {
    pub pkg_id: String,
    /// Short package name derived from `pkg_id` (e.g. `serde`).
    pub pkg_name: String,
    /// Package version derived from `pkg_id` when available.
    pub pkg_version: Option<String>,
    /// The crate name cargo uses (target.name), e.g. `serde_derive`.
    pub name: String,
    pub src_path: String,
    pub edition: String,
    pub crate_types: Vec<String>,
    /// "build" | "run-custom-build" | "check" | "test" | "doc" | ...
    pub mode: String,
    pub features: Vec<String>,
    pub opt_level: String,
    pub debuginfo: u32,
    pub debug_assertions: bool,
    pub incremental: bool,
    pub panic: String,
    pub dependencies: Vec<UnitDep>,
}

#[derive(Debug, Clone)]
pub struct UnitDep {
    pub index: usize,
    pub extern_crate_name: String,
}

// ---------- Raw serde mirror of cargo's JSON schema ----------

#[derive(Debug, Deserialize)]
struct RawUnitGraph {
    #[allow(dead_code)]
    version: u32,
    units: Vec<RawUnit>,
    #[allow(dead_code)]
    #[serde(default)]
    roots: Vec<usize>,
}

#[derive(Debug, Deserialize)]
struct RawUnit {
    pkg_id: String,
    target: RawTarget,
    profile: RawProfile,
    mode: String,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    dependencies: Vec<RawDep>,
}

#[derive(Debug, Deserialize)]
struct RawTarget {
    name: String,
    src_path: String,
    edition: String,
    #[serde(default)]
    crate_types: Vec<String>,
    #[allow(dead_code)]
    #[serde(default)]
    kind: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawProfile {
    opt_level: String,
    // cargo emits debuginfo as either a number (0,1,2) or a string
    // ("none","line-tables-only","limited","full"). Accept both.
    #[serde(default, deserialize_with = "de_debuginfo")]
    debuginfo: u32,
    #[serde(default)]
    debug_assertions: bool,
    #[serde(default)]
    incremental: bool,
    #[serde(default)]
    panic: String,
}

#[derive(Debug, Deserialize)]
struct RawDep {
    index: usize,
    extern_crate_name: String,
}

fn de_debuginfo<'de, D>(de: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let v = serde_json::Value::deserialize(de)?;
    match v {
        serde_json::Value::Number(n) => Ok(n.as_u64().unwrap_or(0) as u32),
        serde_json::Value::String(s) => Ok(match s.as_str() {
            "none" | "false" | "0" => 0,
            "line-tables-only" => 1,
            "limited" | "1" => 1,
            "full" | "true" | "2" => 2,
            _ => 0,
        }),
        serde_json::Value::Bool(b) => Ok(if b { 2 } else { 0 }),
        serde_json::Value::Null => Ok(0),
        _ => Err(D::Error::custom("invalid debuginfo value")),
    }
}

// ---------- Public API ----------

/// Run `cargo +nightly build --unit-graph -Z unstable-options` and parse it.
///
/// This does *not* compile anything. Cargo emits the unit graph to stdout
/// and exits without running rustc when `--unit-graph` is passed.
pub fn fetch_unit_graph(
    manifest_path: Option<&Path>,
    release: bool,
) -> Result<Vec<UnitGraphUnit>, String> {
    let mut cmd = Command::new("cargo");
    cmd.arg("+nightly")
        .arg("build")
        .arg("--unit-graph")
        .arg("-Z")
        .arg("unstable-options");
    if release {
        cmd.arg("--release");
    }
    if let Some(p) = manifest_path {
        cmd.arg("--manifest-path").arg(p);
    }

    let out = cmd
        .output()
        .map_err(|e| format!("spawn cargo: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo +nightly build --unit-graph failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }

    parse_unit_graph(&out.stdout)
}

/// Parse raw JSON bytes into our typed unit list.
pub fn parse_unit_graph(bytes: &[u8]) -> Result<Vec<UnitGraphUnit>, String> {
    let raw: RawUnitGraph =
        serde_json::from_slice(bytes).map_err(|e| format!("parse unit-graph json: {e}"))?;

    let units: Vec<UnitGraphUnit> = raw
        .units
        .into_iter()
        .map(|u| {
            let (pkg_name, pkg_version) = split_pkg_id(&u.pkg_id);
            UnitGraphUnit {
                pkg_id: u.pkg_id,
                pkg_name,
                pkg_version,
                name: u.target.name,
                src_path: u.target.src_path,
                edition: u.target.edition,
                crate_types: u.target.crate_types,
                mode: u.mode,
                features: u.features,
                opt_level: u.profile.opt_level,
                debuginfo: u.profile.debuginfo,
                debug_assertions: u.profile.debug_assertions,
                incremental: u.profile.incremental,
                panic: u.profile.panic,
                dependencies: u
                    .dependencies
                    .into_iter()
                    .map(|d| UnitDep {
                        index: d.index,
                        extern_crate_name: d.extern_crate_name,
                    })
                    .collect(),
            }
        })
        .collect();

    Ok(units)
}

/// Split a cargo pkg_id into (name, version). Handles all three formats
/// cargo emits in the wild:
///
///   * legacy:    `proc-macro2 1.0.106 (registry+https://...)`
///   * registry:  `registry+https://...#proc-macro2@1.0.106`
///   * path/git:  `path+file:///tmp/local#0.1.0` (name = last URL segment)
///
/// Returns `(name, version?)`. For path/git style we fall back to the last
/// non-empty segment of the URL as the name when there is no `name@ver`
/// after the `#`.
fn split_pkg_id(id: &str) -> (String, Option<String>) {
    if let Some((prefix, tail)) = id.rsplit_once('#') {
        // New style. Tail is either `name@version` or just `version`.
        if let Some((name, ver)) = tail.rsplit_once('@') {
            return (name.to_string(), Some(ver.to_string()));
        }
        // Tail is just a version; derive name from the URL segment.
        let name = prefix
            .rsplit(['/', '\\'])
            .find(|s| !s.is_empty())
            .unwrap_or(prefix)
            .to_string();
        let ver = (!tail.is_empty()).then(|| tail.to_string());
        return (name, ver);
    }
    // Legacy style: "name version (source)".
    if let Some((name, rest)) = id.split_once(' ') {
        let ver = rest.split_whitespace().next().map(|s| s.to_string());
        return (name.to_string(), ver);
    }
    (id.to_string(), None)
}

/// Kahn's algorithm over the unit-graph indices.
///
/// `run-custom-build` units (build-script execution) are intentionally
/// kept in the graph: they produce artifacts that `build`-mode units
/// depend on, and skipping them would corrupt the layering.
///
/// Returns phases where each phase is a vector of unit indices (into the
/// input slice) that have no unresolved dependencies. Within a phase,
/// units are sorted by `(pkg_name, name, mode)` for stable output.
pub fn assign_phases(units: &[UnitGraphUnit]) -> Result<Vec<Vec<usize>>, String> {
    let n = units.len();
    let mut indeg = vec![0usize; n];
    // Outgoing edges: for each i, which j's depend on i.
    let mut out: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, u) in units.iter().enumerate() {
        for d in &u.dependencies {
            if d.index >= n {
                return Err(format!(
                    "unit {i} ({}) references out-of-range dep index {}",
                    u.name, d.index
                ));
            }
            indeg[i] += 1;
            out[d.index].push(i);
        }
    }

    let sort_key = |i: &usize| {
        let u = &units[*i];
        (u.pkg_name.clone(), u.name.clone(), u.mode.clone())
    };

    let mut ready: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    ready.sort_by_key(sort_key);

    let mut phases: Vec<Vec<usize>> = Vec::new();
    let mut emitted = 0usize;
    while !ready.is_empty() {
        phases.push(ready.clone());
        emitted += ready.len();
        let mut next: Vec<usize> = Vec::new();
        for &i in &ready {
            for &j in &out[i] {
                indeg[j] -= 1;
                if indeg[j] == 0 {
                    next.push(j);
                }
            }
        }
        next.sort_by_key(sort_key);
        ready = next;
    }

    if emitted != n {
        return Err(format!(
            "unit graph contains a cycle: emitted {emitted} of {n} units"
        ));
    }
    Ok(phases)
}

/// Pretty-print the plan, one phase per block. Mirrors the style of
/// `plan::print` for easy side-by-side comparison.
pub fn print_plan(units: &[UnitGraphUnit], phases: &[Vec<usize>], workspace_only: bool) {
    let total: usize = phases.iter().map(|p| p.len()).sum();
    println!("parallel-rustc plan (unit-graph, v0.3.0)");
    println!("units: {}   phases: {}", total, phases.len());
    println!();

    for (i, phase) in phases.iter().enumerate() {
        // Count build-script execution units separately — they don't
        // produce rlibs and tend to noise up the output.
        let mut build_units: Vec<&UnitGraphUnit> = Vec::new();
        let mut bs_run = 0usize;
        for &idx in phase {
            let u = &units[idx];
            if u.mode == "run-custom-build" {
                bs_run += 1;
            } else {
                build_units.push(u);
            }
        }

        if workspace_only {
            // Without the cargo metadata workspace list, treat
            // "path+file://" ids as workspace members.
            build_units.retain(|u| u.pkg_id.starts_with("path+file://"));
        }

        let label = if bs_run > 0 {
            format!(
                "phase {i} ({} rustc + {bs_run} build-script runs, parallel):",
                build_units.len()
            )
        } else {
            format!("phase {i} ({} units, parallel):", build_units.len())
        };
        println!("{label}");

        for u in &build_units {
            let feat = if u.features.is_empty() {
                String::new()
            } else {
                format!(" [{}]", u.features.join(","))
            };
            let ver = u
                .pkg_version
                .as_deref()
                .map(|v| format!(" v{v}"))
                .unwrap_or_default();
            let mode = if u.mode == "build" {
                String::new()
            } else {
                format!(" ({})", u.mode)
            };
            println!("  - {}{ver}{mode}{feat}", u.name);
        }
    }
}

/// Map a unit to the set of package ids reachable from it (including
/// itself). Useful to reconcile unit-graph output against cargo metadata
/// package-level phase output.
pub fn pkg_ids_by_phase(units: &[UnitGraphUnit], phases: &[Vec<usize>]) -> Vec<Vec<String>> {
    phases
        .iter()
        .map(|phase| {
            let mut set: HashMap<String, ()> = HashMap::new();
            for &idx in phase {
                set.insert(units[idx].pkg_id.clone(), ());
            }
            let mut v: Vec<String> = set.into_keys().collect();
            v.sort();
            v
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_unit(name: &str, mode: &str, deps: &[(usize, &str)]) -> UnitGraphUnit {
        UnitGraphUnit {
            pkg_id: format!("path+file:///tmp/{name}#0.1.0"),
            pkg_name: name.to_string(),
            pkg_version: Some("0.1.0".into()),
            name: name.to_string(),
            src_path: format!("/tmp/{name}/src/lib.rs"),
            edition: "2021".into(),
            crate_types: vec!["lib".into()],
            mode: mode.into(),
            features: vec![],
            opt_level: "0".into(),
            debuginfo: 2,
            debug_assertions: true,
            incremental: false,
            panic: "unwind".into(),
            dependencies: deps
                .iter()
                .map(|(i, n)| UnitDep {
                    index: *i,
                    extern_crate_name: (*n).to_string(),
                })
                .collect(),
        }
    }

    #[test]
    fn phases_chain() {
        // chain_1 <- chain_2 <- chain_3
        let units = vec![
            mk_unit("chain_1", "build", &[]),
            mk_unit("chain_2", "build", &[(0, "chain_1")]),
            mk_unit("chain_3", "build", &[(1, "chain_2")]),
        ];
        let phases = assign_phases(&units).unwrap();
        assert_eq!(phases.len(), 3);
        assert_eq!(phases[0], vec![0]);
        assert_eq!(phases[1], vec![1]);
        assert_eq!(phases[2], vec![2]);
    }

    #[test]
    fn phases_diamond() {
        //    top
        //   /   \
        // left  right
        //   \   /
        //   base
        let units = vec![
            mk_unit("base", "build", &[]),
            mk_unit("left", "build", &[(0, "base")]),
            mk_unit("right", "build", &[(0, "base")]),
            mk_unit("top", "build", &[(1, "left"), (2, "right")]),
        ];
        let phases = assign_phases(&units).unwrap();
        assert_eq!(phases.len(), 3);
        assert_eq!(phases[0], vec![0]);
        assert_eq!(phases[1], vec![1, 2]);
        assert_eq!(phases[2], vec![3]);
    }

    #[test]
    fn parse_minimal_json() {
        let json = br#"{
            "version": 1,
            "units": [
                {
                    "pkg_id": "path+file:///tmp/a#0.1.0",
                    "target": {
                        "name": "a", "src_path": "/tmp/a/src/lib.rs",
                        "edition": "2021", "crate_types": ["lib"], "kind": ["lib"]
                    },
                    "profile": {
                        "opt_level": "0", "debuginfo": 2,
                        "debug_assertions": true, "incremental": false,
                        "panic": "unwind"
                    },
                    "mode": "build", "features": [], "dependencies": []
                }
            ],
            "roots": [0]
        }"#;
        let units = parse_unit_graph(json).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "a");
        assert_eq!(units[0].edition, "2021");
    }

    #[test]
    fn parse_debuginfo_string() {
        let json = br#"{
            "version": 1,
            "units": [
                {
                    "pkg_id": "a@0.1.0",
                    "target": {
                        "name": "a", "src_path": "/tmp/a/src/lib.rs",
                        "edition": "2021", "crate_types": ["lib"], "kind": ["lib"]
                    },
                    "profile": {
                        "opt_level": "0", "debuginfo": "line-tables-only",
                        "debug_assertions": true, "incremental": false,
                        "panic": "unwind"
                    },
                    "mode": "build", "features": [], "dependencies": []
                }
            ],
            "roots": [0]
        }"#;
        let units = parse_unit_graph(json).unwrap();
        assert_eq!(units[0].debuginfo, 1);
    }

    #[test]
    fn split_pkg_id_formats() {
        let (n, v) = split_pkg_id("registry+https://github.com/rust-lang/crates.io-index#proc-macro2@1.0.106");
        assert_eq!(n, "proc-macro2");
        assert_eq!(v.as_deref(), Some("1.0.106"));

        let (n, v) = split_pkg_id("proc-macro2 1.0.106 (registry+https://x)");
        assert_eq!(n, "proc-macro2");
        assert_eq!(v.as_deref(), Some("1.0.106"));

        let (n, v) = split_pkg_id("path+file:///tmp/local#0.1.0");
        assert_eq!(n, "local");
        assert_eq!(v.as_deref(), Some("0.1.0"));
    }
}
