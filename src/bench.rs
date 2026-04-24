//! Benchmark harness: run cold builds in three modes and print a comparison.
//!
//! Modes:
//!   1. serial:            `cargo build -j1`
//!   2. cargo parallel:    `cargo build -jN`
//!   3. parallel-rustc v4: unit-graph driven parallel `cargo build -p`
//!      per phase with per-pkg isolated `CARGO_TARGET_DIR`s.
//!      (see [`crate::builder_v4::run_build_v4`])

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use petgraph::graph::NodeIndex;
use tokio::process::Command;

use crate::builder::BuildConfig;
use crate::builder_v4::run_build_v4;
use crate::graph::Dag;
use crate::metadata::Metadata;

/// Run three build modes sequentially and print a comparison table.
///
/// The `_dag` and `_phases` are accepted for API continuity with callers from
/// v0.1.0 but are no longer used: v4 derives its own phase plan from
/// cargo's unit graph.
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
    println!("  [1/3] serial (-j1)           ...");
    clean_all(config.manifest_path.as_deref()).await?;
    let serial = time_cargo_build(1, config).await?;
    println!("        {:>6.1}s", serial.as_secs_f64());

    // Mode 2: cargo's own parallelism at -jN.
    println!("  [2/3] cargo parallel (-j{})   ...", config.jobs);
    clean_all(config.manifest_path.as_deref()).await?;
    let cargo_par = time_cargo_build(config.jobs, config).await?;
    let sp_ratio = ratio(serial, cargo_par);
    println!(
        "        {:>6.1}s  ({:.2}x faster than serial)",
        cargo_par.as_secs_f64(),
        sp_ratio
    );

    // Mode 3: parallel-rustc v4 (unit-graph driven).
    println!("  [3/3] parallel-rustc v4 (-j{}) ...", config.jobs);
    clean_all(config.manifest_path.as_deref()).await?;
    let v4_started = Instant::now();
    let summary = run_build_v4(config).await?;
    let v4 = v4_started.elapsed();
    let v4_vs_serial = ratio(serial, v4);
    let v4_vs_cargo = ratio(cargo_par, v4);
    println!(
        "        {:>6.1}s  ({:.2}x vs serial, {:.2}x vs cargo)",
        v4.as_secs_f64(),
        v4_vs_serial,
        v4_vs_cargo
    );

    println!();
    println!("Summary:");
    println!("  serial:            {:>5.1}s", serial.as_secs_f64());
    println!(
        "  cargo -j{}:         {:>5.1}s  ({:.2}x)",
        config.jobs,
        cargo_par.as_secs_f64(),
        sp_ratio
    );
    println!(
        "  parallel-rustc v4: {:>5.1}s  ({:.2}x vs serial, {:.2}x vs cargo)",
        v4.as_secs_f64(),
        v4_vs_serial,
        v4_vs_cargo
    );
    println!(
        "  v4 compile-only:   {:>5.1}s  ({:.2}x vs serial, {:.2}x vs cargo)",
        summary.compile.as_secs_f64(),
        ratio(serial, summary.compile),
        ratio(cargo_par, summary.compile),
    );
    println!("  units total:       {:>5}", summary.units);
    println!("  units driven:      {:>5}", summary.driven_units);
    println!("  phases used:       {:>5}", summary.phases);
    println!("  max phase width:   {:>5}", summary.max_phase_width);
    println!();
    println!("Note: v4 total includes unit-graph fetch, seed/merge overhead, and");
    println!("      a final cargo build pass. `compile-only` isolates the time");
    println!("      spent inside parallel phase execution.");

    Ok(())
}

fn ratio(base: Duration, other: Duration) -> f64 {
    if other.as_secs_f64() <= 0.0 {
        return 0.0;
    }
    base.as_secs_f64() / other.as_secs_f64()
}

/// Clean both the workspace's real target dir and the v4 `target-v4`
/// scratch dir, so each mode starts cold.
async fn clean_all(manifest_path: Option<&Path>) -> Result<(), String> {
    cargo_clean(manifest_path).await?;
    if let Some(p) = manifest_path {
        let ws = p.parent().unwrap_or_else(|| Path::new("."));
        let v4 = ws.join("target-v4");
        if v4.exists() {
            let _ = std::fs::remove_dir_all(&v4);
        }
    }
    Ok(())
}

async fn cargo_clean(manifest_path: Option<&Path>) -> Result<(), String> {
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
