//! Benchmark harness: run cold builds in three modes and print a comparison.
//!
//! Modes:
//!   1. serial:         `cargo build -j1`
//!   2. cargo parallel: `cargo build -jN`
//!   3. parallel-rustc: phase-driven via [`crate::builder::run_build`]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use petgraph::graph::NodeIndex;
use tokio::process::Command;

use crate::builder::{run_build, BuildConfig};
use crate::graph::Dag;
use crate::metadata::Metadata;

/// Run all three build modes sequentially and print a comparison table.
pub async fn run_bench(
    meta: &Metadata,
    dag: &Dag,
    phases: &[Vec<NodeIndex>],
    config: &BuildConfig,
) -> Result<(), String> {
    println!("parallel-rustc bench");
    if !meta.workspace_root.is_empty() {
        println!("workspace: {}", meta.workspace_root);
    }
    let profile = if config.release { "release" } else { "debug" };
    println!();
    println!(
        "Running 3 build modes (cold builds, {})...",
        profile
    );
    println!();

    // Mode 1: serial baseline.
    println!("  [1/3] serial (-j1)         ...");
    clean(config.manifest_path.as_deref()).await?;
    let serial = time_cargo_build(1, config).await?;
    println!("        {:>6.1}s", serial.as_secs_f64());

    // Mode 2: cargo's own parallelism at -jN.
    println!("  [2/3] cargo parallel (-j{}) ...", config.jobs);
    clean(config.manifest_path.as_deref()).await?;
    let cargo_par = time_cargo_build(config.jobs, config).await?;
    let sp_ratio = ratio(serial, cargo_par);
    println!(
        "        {:>6.1}s  ({:.2}× faster than serial)",
        cargo_par.as_secs_f64(),
        sp_ratio
    );

    // Mode 3: our phase-driven executor.
    println!("  [3/3] parallel-rustc (-j{}) ...", config.jobs);
    clean(config.manifest_path.as_deref()).await?;
    let prustc_started = Instant::now();
    // Re-use run_build, but silence its own detailed output? For the bench we
    // want the user to see progress too, so we let it print.
    run_build(meta, dag, phases, config).await?;
    let prustc = prustc_started.elapsed();
    let pr_vs_serial = ratio(serial, prustc);
    let pr_vs_cargo = ratio(cargo_par, prustc);
    println!(
        "        {:>6.1}s  ({:.2}× faster than serial, {:.2}× faster than cargo)",
        prustc.as_secs_f64(),
        pr_vs_serial,
        pr_vs_cargo
    );

    let max_phase = phases.iter().map(|p| p.len()).max().unwrap_or(0);

    println!();
    println!("Summary:");
    println!("  serial:          {:>5.1}s", serial.as_secs_f64());
    println!(
        "  cargo -j{}:       {:>5.1}s  ({:.2}×)",
        config.jobs,
        cargo_par.as_secs_f64(),
        sp_ratio
    );
    println!(
        "  parallel-rustc:  {:>5.1}s  ({:.2}× vs serial, {:.2}× vs cargo)",
        prustc.as_secs_f64(),
        pr_vs_serial,
        pr_vs_cargo
    );
    println!("  phases used:     {:>5}", phases.len());
    println!("  max phase width: {:>5}", max_phase);

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
