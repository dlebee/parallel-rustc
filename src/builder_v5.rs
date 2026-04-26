//! v0.5.0 — RUSTC_WRAPPER as build coordinator with metadata pipelining.
//!
//! Implements spec/0.5.0.md:
//!
//! ## Forward pass (cargo build with coordinator)
//!
//! The coordinator intercepts every rustc call:
//! - Lib crates: runs `--emit=dep-info,metadata` only (fast .rmeta), queues
//!   full original args (with env captured from Cargo's environment).
//! - Bin crates: suppresses the compile entirely, queues original args + env.
//! - Build scripts, probes: pass straight through to real rustc.
//!
//! Queue format matches the `parallel-rustc-wrapper` format so we can reuse
//! `recorder::parse_recorded` and `recorder::assign_phases`:
//!   [ cwd, [[env_k, env_v], ...], rustc_path, rustc_arg, ... ]
//!
//! ## Codegen pass (parallel replay)
//!
//! After cargo's forward pass:
//! 1. Parse queue via `recorder::parse_recorded`.
//! 2. Assign phases via `--extern` DAG (`recorder::assign_phases`).
//! 3. Run full rustc (original args, original env) in parallel per phase.
//!    All `.rmeta` exist already so type-checking is instant in each phase.
//!
//! ## Post-processing
//!
//! Cargo normally copies `target/debug/deps/app-HASH` → `target/debug/app`.
//! Since we suppressed the bin compile during the forward pass, cargo skips
//! this step. We replicate it via `link_binaries`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::process::Command;
use tokio::task::JoinSet;

use crate::builder::BuildConfig;
use crate::recorder::{assign_phases, parse_recorded, RustcUnit};

#[derive(Debug, Clone)]
pub struct BuildV5Summary {
    pub total: Duration,
    pub cargo_pass: Duration,
    pub codegen_pass: Duration,
    pub queued_units: usize,
    pub phases: usize,
    pub max_phase_width: usize,
}

/// Locate `parallel-rustc-coordinator` binary.
pub fn locate_coordinator() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("PARALLEL_RUSTC_COORDINATOR_BIN") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Ok(pb);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("parallel-rustc-coordinator");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    for p in [
        "target/release/parallel-rustc-coordinator",
        "target/debug/parallel-rustc-coordinator",
    ] {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Ok(pb);
        }
    }
    Err(
        "could not locate parallel-rustc-coordinator; \
         build with `cargo build --release --bin parallel-rustc-coordinator` \
         or set PARALLEL_RUSTC_COORDINATOR_BIN"
            .to_string(),
    )
}

fn queue_file_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("parallel-rustc-queue-{nonce}.jsonl"))
}

pub async fn run_build_v5(config: &BuildConfig) -> Result<BuildV5Summary, String> {
    let coordinator = locate_coordinator()?;
    let queue_file = queue_file_path();

    println!("parallel-rustc build (v5: RUSTC_WRAPPER metadata pipelining)");
    if let Some(p) = &config.manifest_path {
        println!("manifest: {}", p.display());
    }
    println!(
        "profile: {}   jobs/codegen-phase: {}",
        if config.release { "release" } else { "debug" },
        config.jobs
    );
    println!("coordinator: {}", coordinator.display());
    println!("queue:       {}", queue_file.display());
    println!();

    let _ = std::fs::remove_file(&queue_file);
    let overall = Instant::now();

    // ── Pass 1: Cargo forward pass (metadata only) ──────────────────────────
    println!("[1/3] cargo build (metadata-only forward pass)...");
    let cargo_started = Instant::now();
    run_cargo_with_coordinator(&coordinator, &queue_file, config).await?;
    let cargo_pass = cargo_started.elapsed();
    println!("      done in {:.2}s", cargo_pass.as_secs_f64());

    // ── Pass 2: Parse queue & plan ──────────────────────────────────────────
    println!("[2/3] parsing queue and assigning codegen phases...");
    // Reuse recorder::parse_recorded — same format produced by coordinator.
    let units: Vec<RustcUnit> = parse_recorded(&queue_file)
        .map_err(|e| format!("parse queue: {e}"))?
        .into_iter()
        .filter(|u| !u.crate_name.is_empty() && !u.out_dir.is_empty())
        .collect();

    if units.is_empty() {
        let total = overall.elapsed();
        println!("      queue empty — build was a no-op");
        println!("total: {:.2}s", total.as_secs_f64());
        return Ok(BuildV5Summary {
            total,
            cargo_pass,
            codegen_pass: Duration::ZERO,
            queued_units: 0,
            phases: 0,
            max_phase_width: 0,
        });
    }
    let phases = assign_phases(&units);
    let max_width = phases.iter().map(|p| p.len()).max().unwrap_or(0);
    println!(
        "      {} units   {} phases   max phase width = {}",
        units.len(),
        phases.len(),
        max_width
    );

    // ── Pass 3: Parallel codegen replay ────────────────────────────────────
    println!("[3/3] running full codegen in parallel...");
    let cg_started = Instant::now();
    run_codegen_phases(&units, &phases, config).await?;
    let codegen_pass = cg_started.elapsed();
    println!("      done in {:.2}s", codegen_pass.as_secs_f64());

    // Copy/hardlink bin artifacts to target/{profile}/ (cargo normally does this).
    link_binaries(&units, config.release);

    let total = overall.elapsed();
    println!();
    println!(
        "total: {:.2}s   (cargo: {:.2}s   codegen: {:.2}s)",
        total.as_secs_f64(),
        cargo_pass.as_secs_f64(),
        codegen_pass.as_secs_f64()
    );

    Ok(BuildV5Summary {
        total,
        cargo_pass,
        codegen_pass,
        queued_units: units.len(),
        phases: phases.len(),
        max_phase_width: max_width,
    })
}

async fn run_cargo_with_coordinator(
    coordinator: &Path,
    queue_file: &Path,
    config: &BuildConfig,
) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    if config.release {
        cmd.arg("--release");
    }
    if let Some(p) = &config.manifest_path {
        cmd.arg("--manifest-path").arg(p);
    }
    // Let Cargo use its default -j (parallel .rmeta production for free).
    cmd.env("RUSTC_WRAPPER", coordinator);
    cmd.env("PARALLEL_RUSTC_QUEUE", queue_file);
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());

    let status = cmd
        .status()
        .await
        .map_err(|e| format!("failed to spawn cargo build (v5): {e}"))?;
    if !status.success() {
        return Err(format!("cargo build (v5 forward pass) failed: {status}"));
    }
    Ok(())
}

async fn run_codegen_phases(
    units: &[RustcUnit],
    phases: &[Vec<usize>],
    config: &BuildConfig,
) -> Result<(), String> {
    let concurrency = config.jobs.max(1);

    for (pi, phase) in phases.iter().enumerate() {
        if phase.is_empty() {
            continue;
        }
        let phase_started = Instant::now();
        println!(
            "  phase {pi} ({} unit{})...",
            phase.len(),
            if phase.len() == 1 { "" } else { "s" }
        );

        // Build scripts pass through in the coordinator and are never queued.
        let runnable: Vec<usize> = phase
            .iter()
            .copied()
            .filter(|&idx| !units[idx].should_skip_replay())
            .collect();

        for wave in runnable.chunks(concurrency) {
            let mut set: JoinSet<Result<(), String>> = JoinSet::new();
            for &idx in wave {
                let unit = units[idx].clone();
                set.spawn(async move { run_unit(&unit).await });
            }
            while let Some(res) = set.join_next().await {
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        set.abort_all();
                        return Err(e);
                    }
                    Err(e) => {
                        set.abort_all();
                        return Err(format!("task join: {e}"));
                    }
                }
            }
        }
        println!("    done in {:.2}s", phase_started.elapsed().as_secs_f64());
    }
    Ok(())
}

async fn run_unit(unit: &RustcUnit) -> Result<(), String> {
    if unit.args.is_empty() {
        return Err("empty argv in queued unit".into());
    }

    // Ensure output directories exist (cargo forward pass may not have created
    // them for suppressed bin compiles).
    if !unit.out_dir.is_empty() {
        let _ = std::fs::create_dir_all(&unit.out_dir);
    }
    let mut next_c = false;
    for a in &unit.args {
        if next_c {
            if let Some(rest) = a.strip_prefix("incremental=") {
                let _ = std::fs::create_dir_all(rest);
            }
            next_c = false;
            continue;
        }
        if a == "-C" {
            next_c = true;
        } else if let Some(r) = a.strip_prefix("-Cincremental=") {
            let _ = std::fs::create_dir_all(r);
        } else if let Some(r) = a.strip_prefix("--out-dir=") {
            let _ = std::fs::create_dir_all(r);
        }
    }

    let mut cmd = Command::new(&unit.args[0]);
    cmd.args(&unit.args[1..]);
    if !unit.cwd.is_empty() {
        cmd.current_dir(&unit.cwd);
    }
    // Replay the exact env Cargo had set for this crate (OUT_DIR etc).
    for (k, v) in &unit.env {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());

    let status = cmd
        .status()
        .await
        .map_err(|e| format!("failed to spawn rustc for {}: {e}", unit.label()))?;
    if !status.success() {
        return Err(format!("rustc failed for {}: {status}", unit.label()));
    }
    Ok(())
}

/// After codegen replay, bin artifacts land in `target/{profile}/deps/NAME-HASH`.
/// Cargo normally copies/hardlinks them to `target/{profile}/NAME`. Replicate that.
pub fn link_binaries(units: &[RustcUnit], _release: bool) {
    for unit in units {
        // Bins: --emit contains `link` but not `metadata`, or crate_type is `bin`.
        let is_bin = unit.crate_type == "bin"
            || (unit.crate_type.is_empty()
                && unit.emit.contains("link")
                && !unit.emit.contains("metadata"));
        if !is_bin {
            continue;
        }
        let out_dir = Path::new(&unit.out_dir);
        let profile_dir = match out_dir.parent() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        let crate_name = &unit.crate_name;
        let prefix = format!("{crate_name}-");
        let dest = profile_dir.join(crate_name);

        // Find deps/NAME-HASH (no extension, not .d).
        let bin_in_deps = std::fs::read_dir(out_dir).ok().and_then(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| {
                    let name = p.file_name().unwrap_or_default().to_string_lossy();
                    name.starts_with(&prefix)
                        && !name.ends_with(".d")
                        && p.extension().map_or(true, |ext| ext.is_empty())
                })
        });

        if let Some(src) = bin_in_deps {
            let _ = std::fs::remove_file(&dest);
            if std::fs::hard_link(&src, &dest).is_err() {
                let _ = std::fs::copy(&src, &dest);
            }
        }
    }
}
