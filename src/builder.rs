//! Phase-driven parallel build executor.
//!
//! Two implementations are provided:
//!
//! * [`run_build`] (v0.1.0) — spawns `cargo build -p <name>` concurrently per
//!   phase. Simple, but serialized by Cargo's `target/` lock and does not
//!   guarantee perfect feature unification.
//!
//! * [`run_build_v2`] (v0.2.0) — uses a `RUSTC_WRAPPER` binary
//!   (`parallel-rustc-wrapper`) to record every rustc invocation during a
//!   single serial `cargo build -j1`. After Cargo finishes we `cargo clean`
//!   and replay the recorded invocations in parallel, respecting the DAG we
//!   derive from `--extern` edges. No `target/` lock contention, exact
//!   feature flags, works on stable Rust.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use petgraph::graph::NodeIndex;
use tokio::process::Command;
use tokio::task::JoinSet;

use crate::graph::Dag;
use crate::metadata::{Metadata, Package};
use crate::recorder::{assign_phases, parse_recorded, RustcUnit};

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
    /// If true (v4 batched mode), run one cargo invocation per phase with
    /// multiple `-p` flags and cargo's internal `-j<jobs>` rustc parallelism,
    /// instead of N parallel `cargo -p <pkg> -j1` invocations.
    pub batched: bool,
}

// =============================================================================
// v0.1.0 — per-crate `cargo build -p <name>`
// =============================================================================

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

    println!("parallel-rustc build (v1: per-crate cargo)");
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

async fn run_phase(names: &[String], config: &BuildConfig) -> Result<(), String> {
    let chunk = config.jobs.max(1);
    for wave in names.chunks(chunk) {
        let mut set: JoinSet<Result<(), String>> = JoinSet::new();
        for name in wave {
            let name = name.clone();
            let cfg = config.clone();
            set.spawn(async move { spawn_cargo_build(&name, &cfg).await });
        }
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
    cmd.arg("-j").arg("1");
    if let Some(p) = &config.manifest_path {
        cmd.arg("--manifest-path").arg(p);
    }
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

// =============================================================================
// v0.2.0 — RUSTC_WRAPPER record + parallel replay
// =============================================================================

/// Locate the `parallel-rustc-wrapper` binary.
///
/// Strategy (in order):
/// 1. `$PARALLEL_RUSTC_WRAPPER_BIN` env var (override for tests/dev).
/// 2. Sibling of the current exe (same dir as `parallel-rustc`).
/// 3. `target/release/parallel-rustc-wrapper` relative to cwd.
/// 4. `target/debug/parallel-rustc-wrapper` relative to cwd.


/// Summary of a v0.2.0 build: total wall-clock plus phase stats from the
/// derived DAG. Useful for benchmarks and diagnostics.
#[derive(Debug, Clone)]
pub struct BuildV2Summary {
    pub total: Duration,
    pub replay: Duration,
    pub phases: usize,
    pub max_phase_width: usize,
    pub units: usize,
}

/// True if this recorded unit is an actual compile invocation (not a cargo
/// probe like `rustc -vV` or `rustc --print=...`).
fn is_build_unit(u: &RustcUnit) -> bool {
    !u.crate_name.is_empty() && !u.out_dir.is_empty()
}

pub fn locate_wrapper() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("PARALLEL_RUSTC_WRAPPER_BIN") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Ok(pb);
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join("parallel-rustc-wrapper");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }
    for p in [
        "target/release/parallel-rustc-wrapper",
        "target/debug/parallel-rustc-wrapper",
    ] {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return Ok(pb);
        }
    }
    Err("could not locate parallel-rustc-wrapper binary; build it first with `cargo build --release --bin parallel-rustc-wrapper` or set PARALLEL_RUSTC_WRAPPER_BIN".to_string())
}

/// v0.2.0 parallel build driven by a `RUSTC_WRAPPER` record/replay cycle.
pub async fn run_build_v2(config: &BuildConfig) -> Result<BuildV2Summary, String> {
    let wrapper = locate_wrapper()?;
    let record_file = record_file_path();

    println!("parallel-rustc build (v2: RUSTC_WRAPPER replay)");
    if let Some(p) = &config.manifest_path {
        println!("manifest: {}", p.display());
    }
    println!(
        "profile: {}   jobs/phase: {}",
        if config.release { "release" } else { "debug" },
        config.jobs
    );
    println!("wrapper: {}", wrapper.display());
    println!("record file: {}", record_file.display());
    println!();

    // Start fresh: remove any stale record file.
    let _ = std::fs::remove_file(&record_file);

    let overall = Instant::now();

    // Pass 1 — serial cargo build with the wrapper recording invocations.
    println!("[1/4] recording rustc invocations (cargo build -j1)...");
    let rec_started = Instant::now();
    run_recording_pass(&wrapper, &record_file, config).await?;
    println!("      done in {:.2}s", rec_started.elapsed().as_secs_f64());

    // Parse what we recorded.
    println!("[2/4] parsing recorded invocations...");
    let units: Vec<RustcUnit> = parse_recorded(&record_file)
        .map_err(|e| format!("parse recorded units: {e}"))?
        .into_iter()
        .filter(is_build_unit)
        .collect();
    if units.is_empty() {
        return Err(format!(
            "no rustc invocations were recorded at {} — did the build actually run anything?",
            record_file.display()
        ));
    }
    let phases = assign_phases(&units);
    let max_width = phases.iter().map(|p| p.len()).max().unwrap_or(0);
    println!(
        "      {} units   {} phases   max phase width = {}",
        units.len(),
        phases.len(),
        max_width
    );

    // Pass 2 — selectively clean compilation artifacts but preserve build script
    // outputs (target/*/build/*/out/) because replay invocations may reference
    // files generated by build scripts (e.g. proc_macro2's probe outputs).
    // We clean only .rlib, .rmeta, .d, .so, .dylib, .dll, and bins in deps/.
    println!("[3/4] selective clean (preserving build script outputs)...");
    let clean_started = Instant::now();
    // Remove only target/*/deps/ — keeps build script out/ dirs intact.
    // Build scripts write generated files (code, cfg probes) to out/ which
    // the recorded rustc invocations reference via --include-arg or --out-dir.
    {
        let target_dir = config.manifest_path.as_deref()
            .and_then(|p| p.parent())
            .unwrap_or(std::path::Path::new("."))
            .join("target");
        for profile in &["debug", "release"] {
            let deps = target_dir.join(profile).join("deps");
            if deps.exists() {
                let _ = std::fs::remove_dir_all(&deps);
                let _ = std::fs::create_dir_all(&deps);
            }
        }
    }
    println!("      done in {:.2}s", clean_started.elapsed().as_secs_f64());

    // Pass 3 — parallel replay.
    println!("[4/4] replaying in parallel...");
    let replay_started = Instant::now();
    replay_phases(&units, &phases, config).await?;
    let replay = replay_started.elapsed();
    println!("      replay done in {:.2}s", replay.as_secs_f64());

    let total = overall.elapsed();
    println!();
    println!("total: {:.2}s   (replay-only: {:.2}s)", total.as_secs_f64(), replay.as_secs_f64());
    Ok(BuildV2Summary {
        total,
        replay,
        phases: phases.len(),
        max_phase_width: max_width,
        units: units.len(),
    })
}

/// Variant that skips the recording pass and replays existing units from
/// `record_file`. Useful if a caller (like the bench harness) has already done
/// a recording pass and wants to measure replay in isolation.
pub async fn run_replay_only(
    units: &[RustcUnit],
    config: &BuildConfig,
) -> Result<Duration, String> {
    let phases = assign_phases(units);
    let max_width = phases.iter().map(|p| p.len()).max().unwrap_or(0);
    println!(
        "parallel-rustc replay: {} units, {} phases, max phase width {}",
        units.len(),
        phases.len(),
        max_width
    );
    let started = Instant::now();
    replay_phases(units, &phases, config).await?;
    Ok(started.elapsed())
}

fn record_file_path() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("parallel-rustc-units-{nonce}.jsonl"))
}

async fn run_recording_pass(
    wrapper: &Path,
    record_file: &Path,
    config: &BuildConfig,
) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    if config.release {
        cmd.arg("--release");
    }
    // Force serial so recorded order is a valid topological sort.
    cmd.arg("-j").arg("1");
    if let Some(p) = &config.manifest_path {
        cmd.arg("--manifest-path").arg(p);
    }
    cmd.env("RUSTC_WRAPPER", wrapper);
    cmd.env("PARALLEL_RUSTC_RECORD_FILE", record_file);
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());

    let status = cmd
        .status()
        .await
        .map_err(|e| format!("failed to spawn recording cargo build: {e}"))?;
    if !status.success() {
        return Err(format!("recording cargo build failed with {status}"));
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

async fn replay_phases(
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
            "phase {pi} ({} unit{})...",
            phase.len(),
            if phase.len() == 1 { "" } else { "s" }
        );

        // Chunk so we don't spawn more than `concurrency` at once.
        // Skip probe and build-script units — they were already handled
        // by Cargo in the record pass and must not be replayed.
        let replayable: Vec<usize> = phase.iter().copied()
            .filter(|&idx| !units[idx].should_skip_replay())
            .collect();

        for wave in replayable.chunks(concurrency) {
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
                    Err(join_err) => {
                        set.abort_all();
                        return Err(format!("task join error: {join_err}"));
                    }
                }
            }
        }
        println!(
            "  done in {:.2}s",
            phase_started.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

async fn run_unit(unit: &RustcUnit) -> Result<(), String> {
    // unit.args[0] = rustc binary, rest = args.
    if unit.args.is_empty() {
        return Err("empty rustc argv in recorded unit".into());
    }

    // cargo clean removed target/; recreate directories rustc will write to.
    if !unit.out_dir.is_empty() {
        let _ = std::fs::create_dir_all(&unit.out_dir);
    }
    // Also create -C incremental=<dir> if present, and a few other paths.
    let mut next_is_flag_value = false;
    for a in &unit.args {
        if next_is_flag_value {
            if let Some(rest) = a.strip_prefix("incremental=") {
                let _ = std::fs::create_dir_all(rest);
            }
            next_is_flag_value = false;
            continue;
        }
        if a == "-C" {
            next_is_flag_value = true;
        } else if let Some(rest) = a.strip_prefix("-Cincremental=") {
            let _ = std::fs::create_dir_all(rest);
        } else if let Some(rest) = a.strip_prefix("--out-dir=") {
            let _ = std::fs::create_dir_all(rest);
        }
    }

    let mut cmd = Command::new(&unit.args[0]);
    cmd.args(&unit.args[1..]);
    if !unit.cwd.is_empty() {
        cmd.current_dir(&unit.cwd);
    }
    // Replay the exact env cargo had set for this invocation.
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
