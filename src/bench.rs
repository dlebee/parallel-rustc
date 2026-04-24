//! Benchmark harness: run cold builds in three modes and print a comparison.
//!
//! Modes:
//!   1. serial:           `cargo build -j1`
//!   2. cargo parallel:   `cargo build -jN`
//!   3. parallel-rustc v2: RUSTC_WRAPPER record + parallel replay
//!      (see [`crate::builder::run_build_v2`]).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use petgraph::graph::NodeIndex;
use tokio::process::Command;

use crate::builder::{run_build_v2, BuildConfig};
use crate::graph::Dag;
use crate::metadata::Metadata;

/// Run three build modes sequentially and print a comparison table.
///
/// The `_dag` and `_phases` are accepted for API continuity with callers from
/// v0.1.0 but are no longer used: v0.2.0 derives its own phase plan from
/// recorded rustc invocations.
pub async fn run_bench(
    meta: &Metadata,
    _dag: &Dag,
    _phases: &[Vec<NodeIndex>],
    config: &BuildConfig,
) -> Result<(), String> {
    println!("parallel-rustc bench");
    if !meta.workspace_root.is_empty() {
        println!("workspace: {}", meta.workspace_root);
    }
    let profile = if config.release { "release" } else { "debug" };
    println!();
    println!("Running 3 build modes (cold builds, {})...", profile);
    println!();

    // Mode 1: serial baseline.
    println!("  [1/3] serial (-j1)          ...");
    clean(config.manifest_path.as_deref()).await?;
    let serial = time_cargo_build(1, config).await?;
    println!("        {:>6.1}s", serial.as_secs_f64());

    // Mode 2: cargo's own parallelism at -jN.
    println!("  [2/3] cargo parallel (-j{})  ...", config.jobs);
    clean(config.manifest_path.as_deref()).await?;
    let cargo_par = time_cargo_build(config.jobs, config).await?;
    let sp_ratio = ratio(serial, cargo_par);
    println!(
        "        {:>6.1}s  ({:.2}× faster than serial)",
        cargo_par.as_secs_f64(),
        sp_ratio
    );

    // Mode 3: parallel-rustc v2 (wrapper-based replay).
    println!("  [3/3] parallel-rustc v2 (-j{}) ...", config.jobs);
    clean(config.manifest_path.as_deref()).await?;
    let v2_started = Instant::now();
    let summary = run_build_v2(config).await?;
    let v2 = v2_started.elapsed();
    let v2_vs_serial = ratio(serial, v2);
    let v2_vs_cargo = ratio(cargo_par, v2);
    println!(
        "        {:>6.1}s  ({:.2}× faster than serial, {:.2}× faster than cargo)",
        v2.as_secs_f64(),
        v2_vs_serial,
        v2_vs_cargo
    );

    println!();
    println!("Summary:");
    println!("  serial:            {:>5.1}s", serial.as_secs_f64());
    println!(
        "  cargo -j{}:         {:>5.1}s  ({:.2}×)",
        config.jobs,
        cargo_par.as_secs_f64(),
        sp_ratio
    );
    println!(
        "  parallel-rustc v2: {:>5.1}s  ({:.2}× vs serial, {:.2}× vs cargo)",
        v2.as_secs_f64(),
        v2_vs_serial,
        v2_vs_cargo
    );
    println!("  v2 replay-only:    {:>5.1}s  ({:.2}× vs serial, {:.2}× vs cargo)",
        summary.replay.as_secs_f64(),
        ratio(serial, summary.replay),
        ratio(cargo_par, summary.replay),
    );
    println!("  units compiled:    {:>5}", summary.units);
    println!("  phases used:       {:>5}", summary.phases);
    println!("  max phase width:   {:>5}", summary.max_phase_width);
    println!();
    println!("Note: v2 total includes a serial recording pass AND the parallel");
    println!("      replay. For a fair apples-to-apples against cargo -jN, look");
    println!("      at the \"replay-only\" timing printed above.");

    Ok(())
}

fn ratio(base: Duration, other: Duration) -> f64 {
    if other.as_secs_f64() <= 0.0 {
        return 0.0;
    }
    base.as_secs_f64() / other.as_secs_f64()
}

async fn clean(manifest_path: Option<&Path>) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.arg("clean");
    if let Some(p) = manifest_path {
        cmd.arg("--manifest-path").arg(p);
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::inherit());
    let status = cmd
        .status()
        .await
        .map_err(|e| format!("failed to spawn cargo clean: {e}"))?;
    if !status.success() {
        return Err(format!("cargo clean failed with {status}"));
    }
    Ok(())
}

/// Run `cargo build -j<jobs>` in the given workspace and return wall-clock time.
async fn time_cargo_build(jobs: usize, config: &BuildConfig) -> Result<Duration, String> {
    let manifest: Option<PathBuf> = config.manifest_path.clone();
    let started = Instant::now();
    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    if config.release {
        cmd.arg("--release");
    }
    cmd.arg("-j").arg(jobs.to_string());
    if let Some(p) = &manifest {
        cmd.arg("--manifest-path").arg(p);
    }
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    let status = cmd
        .status()
        .await
        .map_err(|e| format!("failed to spawn cargo build: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build -j{jobs} failed with {status}"));
    }
    Ok(started.elapsed())
}
