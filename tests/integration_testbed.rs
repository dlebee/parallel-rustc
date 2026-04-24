//! Local smoke tests against the parallel-rustc-testbed workspace.
//!
//! These tests build the `parallel-rustc` binary, then run `build` and `bench`
//! against the sibling `parallel-rustc-testbed` workspace. They only run when
//! that workspace is present (typical on the maintainer's machine / CI); on
//! other checkouts they silently pass via `#[ignore]`-style early return so we
//! never fail a fresh clone.
//!
//! Run manually with:
//!   cargo test --test integration_testbed -- --nocapture

use std::path::PathBuf;
use std::process::Command;



// The testbed tests mutate a shared target/ directory, so serialize them with
// a module-level mutex. Cargo runs tests in parallel threads by default, and
// two concurrent builds would corrupt each other's artifacts.
use std::sync::{Mutex, OnceLock};
fn testbed_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn testbed_manifest() -> Option<PathBuf> {
    // Try a couple of plausible locations relative to this repo.
    let candidates = [
        // Sibling checkout layout (what we use on maintainer-node-1).
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("parallel-rustc-testbed")
            .join("Cargo.toml"),
        // Env override.
        std::env::var("PARALLEL_RUSTC_TESTBED")
            .ok()
            .map(PathBuf::from)
            .unwrap_or_default(),
    ];
    for c in candidates {
        if c.as_os_str().is_empty() {
            continue;
        }
        if c.exists() {
            return Some(c);
        }
    }
    None
}

fn binary_path() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo for integration tests.
    PathBuf::from(env!("CARGO_BIN_EXE_parallel-rustc"))
}

fn run_bin(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(binary_path())
        .args(args)
        .output()
        .expect("failed to invoke parallel-rustc binary");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn skip_if_no_testbed() -> Option<PathBuf> {
    match testbed_manifest() {
        Some(p) => Some(p),
        None => {
            eprintln!(
                "skipping testbed smoke test: parallel-rustc-testbed not found (sibling checkout)"
            );
            None
        }
    }
}

/// Ensure the testbed compiles cleanly via `parallel-rustc build`.
#[test]
fn build_against_testbed() {
    let _g = testbed_lock().lock().unwrap_or_else(|e| e.into_inner());
    let Some(manifest) = skip_if_no_testbed() else {
        return;
    };
    let manifest_str = manifest.to_string_lossy().into_owned();

    // v2 needs a cold build — clean so cargo actually records rustc invocations.
    // Without this, if the testbed is already built, cargo records nothing and
    // the replay fails with "no rustc invocations recorded".
    let ws = manifest.parent().unwrap();
    let _ = std::fs::remove_dir_all(ws.join("target"));
    let _ = std::fs::remove_dir_all(ws.join("target-v4"));
    let (ok, stdout, stderr) = run_bin(&["build", "--manifest-path", &manifest_str]);
    assert!(
        ok,
        "parallel-rustc build failed.\nstdout:\n{}\nstderr:\n{}",
        stdout, stderr
    );
    assert!(
        stdout.contains("parallel-rustc build"),
        "missing header in build output:\n{}",
        stdout
    );
    assert!(
        stdout.contains("total:"),
        "missing total timing in build output:\n{}",
        stdout
    );
    // The testbed has a clear multi-phase shape; we should see at least 2 phases.
    let phase_lines = stdout.lines().filter(|l| l.starts_with("phase ")).count();
    assert!(
        phase_lines >= 2,
        "expected at least 2 phase log lines in build output, got {phase_lines}:\n{stdout}"
    );
}

/// Ensure the bench subcommand runs all 3 modes and prints the comparison.
#[test]
fn bench_against_testbed() {
    let _g = testbed_lock().lock().unwrap_or_else(|e| e.into_inner());
    let Some(manifest) = skip_if_no_testbed() else {
        return;
    };
    let manifest_str = manifest.to_string_lossy().into_owned();
    let (ok, stdout, stderr) = run_bin(&["bench", "--manifest-path", &manifest_str]);
    assert!(
        ok,
        "parallel-rustc bench failed.\nstdout:\n{}\nstderr:\n{}",
        stdout, stderr
    );
    for needle in [
        "parallel-rustc bench",
        "[1/3] serial",
        "[2/3] cargo parallel",
        "[3/3] parallel-rustc",
        "Summary:",
        "phases used:",
        "max phase width:",
    ] {
        assert!(
            stdout.contains(needle),
            "bench output missing expected fragment `{}`:\n{}",
            needle,
            stdout
        );
    }
}

/// Guard: make sure the env-override path works when set (doesn't actually run
/// the binary, just confirms our helper doesn't blow up on junk input).
#[test]
fn manifest_discovery_is_defensive() {
    let _g = testbed_lock().lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("PARALLEL_RUSTC_TESTBED", "/definitely/does/not/exist");
    // Clear after the call so other tests aren't affected.
    let manifest = testbed_manifest();
    std::env::remove_var("PARALLEL_RUSTC_TESTBED");
    // We might still find it via the sibling path; that's fine. We only care
    // that the lookup itself didn't panic on an invalid path.
    let _ = manifest.as_ref().map(|p| p.clone());
}

/// Ensure the testbed compiles cleanly via `parallel-rustc build-v4`.
#[test]
fn build_v4_against_testbed() {
    let _g = testbed_lock().lock().unwrap_or_else(|e| e.into_inner());
    let Some(manifest) = skip_if_no_testbed() else {
        return;
    };
    let manifest_str = manifest.to_string_lossy().into_owned();

    // Clean first so v4 sees a cold build.
    let ws = manifest.parent().unwrap();
    let _ = std::fs::remove_dir_all(ws.join("target"));
    let _ = std::fs::remove_dir_all(ws.join("target-v4"));

    let (ok, stdout, stderr) = run_bin(&["build-v4", "--manifest-path", &manifest_str]);
    assert!(
        ok,
        "parallel-rustc build-v4 failed.\nstdout:\n{}\nstderr:\n{}",
        stdout, stderr
    );
    // Header
    assert!(
        stdout.contains("parallel-rustc build (v4"),
        "missing v4 header in output:\n{}",
        stdout
    );
    // Final link summary
    assert!(
        stdout.contains("total:"),
        "missing total timing in output:\n{}",
        stdout
    );
    // At least 2 phases logged
    let phase_lines = stdout.lines().filter(|l| l.trim().starts_with("phase ")).count();
    assert!(
        phase_lines >= 2,
        "expected at least 2 phase log lines, got {phase_lines}:\n{stdout}"
    );
    // Binary must exist
    let bin = ws.join("target").join("debug").join("app");
    assert!(
        bin.exists(),
        "expected app binary at {} after build-v4",
        bin.display()
    );
}
