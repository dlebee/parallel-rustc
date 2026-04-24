//! Phase-driven parallel build executor.
//!
//! Spawns `cargo build -p <name>` concurrently for every crate in a phase,
//! waits for the phase to finish, then moves to the next. Wall-clock time
//! is tracked per phase and in total.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use petgraph::graph::NodeIndex;
use tokio::process::Command;
use tokio::task::JoinSet;

use crate::graph::Dag;
use crate::metadata::{Metadata, Package};

/// Configuration for a `parallel-rustc build` run.
#[derive(Debug, Clone)]
pub struct BuildConfig {
    pub manifest_path: Option<PathBuf>,
    pub release: bool,
    /// Max parallel processes per phase.
    pub jobs: usize,
    /// If true, only build workspace members (external deps still built implicitly
    /// by cargo when it resolves a workspace member, but we don't try to drive them).
    pub workspace_only: bool,
}

/// Execute the phase plan. Returns total wall-clock time on success.
pub async fn run_build(
    meta: &Metadata,
    dag: &Dag,
    phases: &[Vec<NodeIndex>],
    config: &BuildConfig,
) -> Result<Duration, String> {
    let by_id: HashMap<&str, &Package> =
        meta.packages.iter().map(|p| (p.id.as_str(), p)).collect();
    let ws_members: HashSet<&str> =
        meta.workspace_members.iter().map(|s| s.as_str()).collect();

    println!("parallel-rustc build");
    if !meta.workspace_root.is_empty() {
        println!("workspace: {}", meta.workspace_root);
    }
    println!(
        "profile: {}   jobs/phase: {}   phases: {}",
        if config.release { "release" } else { "debug" },
        config.jobs,
        phases.len()
    );
    println!();

    let overall = Instant::now();

    for (i, phase) in phases.iter().enumerate() {
        // Collect crate names to build in this phase.
        let mut names: Vec<String> = Vec::new();
        for &idx in phase {
            let id = dag.id_of(idx);
            if config.workspace_only && !ws_members.contains(id) {
                continue;
            }
            if let Some(pkg) = by_id.get(id) {
                names.push(pkg.name.clone());
            }
        }

        if names.is_empty() {
            continue;
        }

        println!(
            "phase {} ({} package{})...",
            i,
            names.len(),
            if names.len() == 1 { "" } else { "s" }
        );
        let phase_started = Instant::now();

        run_phase(&names, config).await?;

        let elapsed = phase_started.elapsed();
        println!("  done in {:.2}s", elapsed.as_secs_f64());
    }

    let total = overall.elapsed();
    println!();
    println!("total: {:.2}s", total.as_secs_f64());
    Ok(total)
}

/// Spawn `cargo build -p <name>` for each package name, up to `config.jobs`
/// concurrently. Fails fast: the first error aborts remaining tasks.
async fn run_phase(names: &[String], config: &BuildConfig) -> Result<(), String> {
    // Chunk into waves of `config.jobs` so we don't unbounded-spawn.
    let chunk = config.jobs.max(1);
    for wave in names.chunks(chunk) {
        let mut set: JoinSet<Result<(), String>> = JoinSet::new();
        for name in wave {
            let name = name.clone();
            let cfg = config.clone();
            set.spawn(async move { spawn_cargo_build(&name, &cfg).await });
        }
        // Drain the wave; fail fast on first error.
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    set.abort_all();
                    return Err(e);
                }
                Err(join_err) => {
                    set.abort_all();
                    return Err(format!("task join error: {join_err}"));
                }
            }
        }
    }
    Ok(())
}

async fn spawn_cargo_build(pkg: &str, config: &BuildConfig) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.arg("build").arg("-p").arg(pkg);
    if config.release {
        cmd.arg("--release");
    }
    // Each child gets a single cargo job slot; we do the parallelism at the
    // phase level, but cargo still needs at least 1.
    cmd.arg("-j").arg("1");
    if let Some(p) = &config.manifest_path {
        cmd.arg("--manifest-path").arg(p);
    }
    // Stream output so the user sees progress.
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());

    let status = cmd
        .status()
        .await
        .map_err(|e| format!("failed to spawn cargo build -p {pkg}: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build -p {pkg} failed with {status}"));
    }
    Ok(())
}
