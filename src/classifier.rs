//! v0.5.1 — Unit-graph pre-classifier for the metadata-pipelining coordinator.
//!
//! Background: in v0.5.0 the RUSTC_WRAPPER coordinator could not tell, at the
//! moment Cargo invoked rustc on a given crate, whether that crate would be
//! *linked* later in the same forward pass (build-script binary, proc-macro
//! dylib) or only consumed for type-checking. Linking needs `.rlib`,
//! type-checking only needs `.rmeta`. Getting it wrong → "extern location
//! does not exist" errors.
//!
//! The fix: before `cargo build` runs, query the unit-graph
//! (`cargo +nightly build --unit-graph -Z unstable-options`) and walk it
//! transitively from every unit that *will* be linked during the forward
//! pass, marking those units (and their lib dependencies) as "needs full
//! rlib". The coordinator then passes those crate names straight through to
//! the real rustc, and only deferable units go through the metadata-only +
//! queue-and-replay path.
//!
//! "Will be linked during the forward pass" =
//!   * `mode == "run-custom-build"`  — build-script *execution*, links its lib deps
//!   * `mode == "build"` AND `target.kind` contains `"proc-macro"` — proc-macro dylib
//!   * `mode == "build"` AND `target.kind` contains `"bin"` — binary link step
//!   * `mode == "build"` AND `target.kind` contains `"cdylib"` / `"dylib"` / `"staticlib"`
//!
//! From those seeds we walk *backwards through dependency edges* (each unit's
//! `dependencies[].index` points to a unit it consumes), marking everything
//! reachable. The set of `target.name`s (with `-` → `_`) is the needs-rlib
//! set passed to the coordinator via `PARALLEL_RUSTC_NEEDS_RLIB`.
//!
//! Output also includes the complementary `defer_codegen` set for diagnostics.

use std::collections::HashSet;
use std::path::Path;

use crate::unit_graph::{fetch_unit_graph, UnitGraphUnit};

/// Result of pre-classifying a unit graph.
#[derive(Debug, Clone)]
pub struct ClassifiedUnits {
    /// Crate names (underscore-normalized) that need a real `.rlib` produced
    /// during Cargo's forward pass — pass straight through to rustc.
    pub needs_rlib: HashSet<String>,
    /// Crate names safe to defer (metadata-only forward pass + parallel
    /// codegen replay afterwards).
    pub defer_codegen: HashSet<String>,
    /// Total number of units cargo planned (informational).
    pub total_units: usize,
}

/// Run cargo's unit-graph for the given workspace and classify each unit
/// as needs-rlib or defer-codegen.
pub fn classify_from_unit_graph(
    manifest_path: Option<&Path>,
    release: bool,
) -> Result<ClassifiedUnits, String> {
    let units = fetch_unit_graph(manifest_path, release)
        .map_err(|e| format!("fetch unit-graph for classification: {e}"))?;
    Ok(classify_units(&units))
}

/// Pure classifier — exposed separately for unit testing.
pub fn classify_units(units: &[UnitGraphUnit]) -> ClassifiedUnits {
    let n = units.len();

    // Seed: units that *will* be linked during cargo's forward pass.
    let mut needs_rlib_idx = vec![false; n];
    for (i, u) in units.iter().enumerate() {
        if is_link_seed(u) {
            needs_rlib_idx[i] = true;
        }
    }

    // BFS through dependency edges. For each linker unit, add every unit it
    // (transitively) consumes — those crates produce artifacts the linker
    // needs as `.rlib`, so the coordinator must not defer them.
    let mut stack: Vec<usize> = (0..n).filter(|&i| needs_rlib_idx[i]).collect();
    while let Some(i) = stack.pop() {
        for dep in &units[i].dependencies {
            if dep.index < n && !needs_rlib_idx[dep.index] {
                needs_rlib_idx[dep.index] = true;
                stack.push(dep.index);
            }
        }
    }

    // Project to crate names (underscore-normalized — that's how rustc sees
    // them via `--crate-name`). We *only* take the name of "build"-mode units;
    // run-custom-build units are not crate compiles in their own right (they
    // run the build-script binary), and the coordinator never sees their
    // `--crate-name` because it's the same as the lib they belong to.
    let mut needs_rlib: HashSet<String> = HashSet::new();
    let mut defer_codegen: HashSet<String> = HashSet::new();
    for (i, u) in units.iter().enumerate() {
        if u.mode != "build" {
            continue;
        }
        let cname = normalize_crate_name(&u.name);
        if needs_rlib_idx[i] {
            needs_rlib.insert(cname);
        } else {
            defer_codegen.insert(cname);
        }
    }

    // Belt-and-braces: if a name appears in both sets (e.g. two units of the
    // same crate name with different feature sets — rare but possible),
    // needs_rlib wins. Defer is an *opt-in* optimization.
    for n in &needs_rlib {
        defer_codegen.remove(n);
    }

    ClassifiedUnits {
        needs_rlib,
        defer_codegen,
        total_units: n,
    }
}

fn is_link_seed(u: &UnitGraphUnit) -> bool {
    if u.mode == "run-custom-build" {
        return true;
    }
    if u.mode == "build" {
        // proc-macro dylibs are loaded at compile time → need full .rlib
        if u.kind.iter().any(|k| k == "proc-macro") {
            return true;
        }
        // Binary targets link their dependencies during cargo's forward pass.
        // Their lib deps need full .rlib before the link step runs.
        if u.kind.iter().any(|k| k == "bin") {
            return true;
        }
        // cdylib / dylib / staticlib all link their dependencies too.
        if u.kind.iter().any(|k| k == "cdylib" || k == "dylib" || k == "staticlib") {
            return true;
        }
    }
    false
}

/// rustc passes crate names with `-` rewritten to `_` (it's not a valid
/// identifier char). Match that convention so the coordinator's
/// `--crate-name` lookup hits.
fn normalize_crate_name(name: &str) -> String {
    name.replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::unit_graph::{UnitDep, UnitGraphUnit};

    fn mk(name: &str, mode: &str, kind: &[&str], deps: &[usize]) -> UnitGraphUnit {
        UnitGraphUnit {
            pkg_id: format!("path+file:///tmp/{name}#0.1.0"),
            pkg_name: name.to_string(),
            pkg_version: Some("0.1.0".into()),
            name: name.to_string(),
            src_path: format!("/tmp/{name}/src/lib.rs"),
            edition: "2021".into(),
            crate_types: vec!["lib".into()],
            kind: kind.iter().map(|s| s.to_string()).collect(),
            mode: mode.into(),
            features: vec![],
            opt_level: "0".into(),
            debuginfo: 2,
            debug_assertions: true,
            incremental: false,
            panic: "unwind".into(),
            dependencies: deps
                .iter()
                .map(|&i| UnitDep {
                    index: i,
                    extern_crate_name: format!("dep_{i}"),
                })
                .collect(),
        }
    }

    #[test]
    fn proc_macro_pulls_deps_into_needs_rlib() {
        // 0: proc_macro2 (lib)
        // 1: quote       (lib, deps -> 0)
        // 2: my_derive   (proc-macro lib, deps -> 0, 1)
        // 3: leaf        (lib, no link involvement)
        let units = vec![
            mk("proc_macro2", "build", &["lib"], &[]),
            mk("quote", "build", &["lib"], &[0]),
            mk("my_derive", "build", &["proc-macro"], &[0, 1]),
            mk("leaf", "build", &["lib"], &[]),
        ];
        let c = classify_units(&units);
        assert!(c.needs_rlib.contains("proc_macro2"));
        assert!(c.needs_rlib.contains("quote"));
        assert!(c.needs_rlib.contains("my_derive"));
        assert!(c.defer_codegen.contains("leaf"));
        assert!(!c.needs_rlib.contains("leaf"));
    }

    #[test]
    fn build_script_pulls_deps_into_needs_rlib() {
        // 0: version_check (lib, no proc-macro, no /build/ path — pure lib)
        // 1: build script run for crate X (mode=run-custom-build, deps -> 0)
        // 2: X itself (lib, deps -> 1 — X depends on build script having run)
        // 3: unrelated_lib
        let units = vec![
            mk("version_check", "build", &["lib"], &[]),
            mk("build-script-build", "run-custom-build", &["custom-build"], &[0]),
            mk("x", "build", &["lib"], &[1]),
            mk("unrelated_lib", "build", &["lib"], &[]),
        ];
        let c = classify_units(&units);
        // version_check is consumed by a build-script execution → needs full rlib.
        assert!(c.needs_rlib.contains("version_check"));
        assert!(c.defer_codegen.contains("unrelated_lib"));
    }

    #[test]
    fn dash_in_name_normalized_to_underscore() {
        let units = vec![
            mk("proc-macro2", "build", &["lib"], &[]),
            mk("my-derive", "build", &["proc-macro"], &[0]),
        ];
        let c = classify_units(&units);
        assert!(c.needs_rlib.contains("proc_macro2"));
        assert!(c.needs_rlib.contains("my_derive"));
    }

    #[test]
    fn all_lib_no_seeds_means_all_deferred() {
        let units = vec![
            mk("a", "build", &["lib"], &[]),
            mk("b", "build", &["lib"], &[0]),
        ];
        let c = classify_units(&units);
        assert!(c.needs_rlib.is_empty());
        assert!(c.defer_codegen.contains("a"));
        assert!(c.defer_codegen.contains("b"));
    }

    #[test]
    fn bin_target_pulls_all_lib_deps_into_needs_rlib() {
        // 0: libfoo (lib)
        // 1: libbar (lib, deps -> 0)
        // 2: mybin  (bin, deps -> 1)   ← link seed
        // 3: unrelated (lib)
        let units = vec![
            mk("libfoo", "build", &["lib"], &[]),
            mk("libbar", "build", &["lib"], &[0]),
            mk("mybin", "build", &["bin"], &[1]),
            mk("unrelated", "build", &["lib"], &[]),
        ];
        let c = classify_units(&units);
        // bin and its transitive lib deps all need full rlib
        assert!(c.needs_rlib.contains("mybin"));
        assert!(c.needs_rlib.contains("libbar"));
        assert!(c.needs_rlib.contains("libfoo"));
        // unrelated lib with no link consumer can be deferred
        assert!(c.defer_codegen.contains("unrelated"));
        assert!(!c.needs_rlib.contains("unrelated"));
    }
}
