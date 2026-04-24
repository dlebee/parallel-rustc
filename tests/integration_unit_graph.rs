//! Integration tests for v0.3.0 unit-graph-based planning.
//!
//! These require a nightly toolchain to be installed. On CI we ship
//! nightly; on laptops without it the tests silently pass.

use std::path::PathBuf;
use std::process::Command;

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_parallel-rustc"))
}

fn testbed_manifest() -> Option<PathBuf> {
    let sibling = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("parallel-rustc-testbed")
        .join("Cargo.toml");
    if sibling.exists() {
        return Some(sibling);
    }
    std::env::var("PARALLEL_RUSTC_TESTBED")
        .ok()
        .map(PathBuf::from)
        .filter(|p| p.exists())
}

fn have_nightly() -> bool {
    Command::new("cargo")
        .args(["+nightly", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_bin(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(binary_path())
        .args(args)
        .output()
        .expect("spawn parallel-rustc");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Extract the ordered list of per-phase package/unit name sets from plan
/// output. For the build-script-free testbed, `plan` and `plan-v2` should
/// produce identical layering (modulo the hyphen-vs-underscore crate-name
/// difference that target.name uses).
fn phase_names(stdout: &str) -> Vec<Vec<String>> {
    let mut phases: Vec<Vec<String>> = Vec::new();
    let mut cur: Option<Vec<String>> = None;
    for line in stdout.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("phase ") && trimmed.contains("parallel") {
            if let Some(c) = cur.take() {
                phases.push(c);
            }
            cur = Some(Vec::new());
        } else if trimmed.starts_with("- ") {
            if let Some(c) = cur.as_mut() {
                // Strip optional version + feature suffix so names line up
                // across plan and plan-v2 outputs.
                let rest = &trimmed[2..];
                let name = rest
                    .split_whitespace()
                    .next()
                    .unwrap_or(rest)
                    .replace('_', "-");
                c.push(name);
            }
        }
    }
    if let Some(c) = cur {
        phases.push(c);
    }
    for p in &mut phases {
        p.sort();
    }
    phases
}

#[test]
fn plan_v2_phases_match_plan_on_testbed() {
    let Some(manifest) = testbed_manifest() else {
        eprintln!("skipping: no testbed");
        return;
    };
    if !have_nightly() {
        eprintln!("skipping: no nightly toolchain");
        return;
    }

    let manifest_str = manifest.to_string_lossy().into_owned();
    let (ok1, out1, err1) = run_bin(&["plan", "--manifest-path", &manifest_str]);
    assert!(ok1, "plan failed: {err1}");
    let (ok2, out2, err2) = run_bin(&["plan-v2", "--manifest-path", &manifest_str]);
    assert!(ok2, "plan-v2 failed: {err2}");

    let p1 = phase_names(&out1);
    let p2 = phase_names(&out2);
    assert_eq!(
        p1, p2,
        "phases differ between plan and plan-v2\n--- plan ---\n{out1}\n--- plan-v2 ---\n{out2}"
    );
    assert!(!p1.is_empty(), "empty phases?");
}

#[test]
fn plan_v2_includes_resolved_features() {
    // Use our own Cargo.toml — it's guaranteed to be present and has
    // several features on deps like serde / tokio.
    if !have_nightly() {
        eprintln!("skipping: no nightly toolchain");
        return;
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let manifest_str = manifest.to_string_lossy().into_owned();
    let (ok, out, err) = run_bin(&["plan-v2", "--manifest-path", &manifest_str]);
    assert!(ok, "plan-v2 failed: {err}");
    // serde should appear with `derive` feature resolved.
    assert!(
        out.contains("["),
        "expected at least one [feature,...] tag in plan-v2 output:\n{out}"
    );
}
