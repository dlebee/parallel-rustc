//! v0.5.0 — RUSTC_WRAPPER with metadata-first pipelining + parallel codegen replay.
//!
//! 1. cargo build with RUSTC_WRAPPER=coordinator:
//!    - coordinator emits .rmeta only (synchronous, fast)
//!    - queues original full-emit argv for later
//! 2. After cargo finishes: replay queued invocations in parallel (DAG phases)

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::process::Command;
use tokio::task::JoinSet;

use crate::builder::BuildConfig;
use crate::recorder::{assign_phases, RustcUnit};

#[derive(Debug, Clone)]
pub struct BuildV5Summary {
    pub total: Duration,
    pub cargo_pass: Duration,
    pub codegen_pass: Duration,
    pub queued_units: usize,
    pub phases: usize,
    pub max_phase_width: usize,
}

pub fn locate_coordinator() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("PARALLEL_RUSTC_COORDINATOR_BIN") {
        let pb = PathBuf::from(p);
        if pb.exists() { return Ok(pb); }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let c = dir.join("parallel-rustc-coordinator");
            if c.exists() { return Ok(c); }
        }
    }
    for sub in &["target/release", "target/debug"] {
        let c = PathBuf::from(sub).join("parallel-rustc-coordinator");
        if c.exists() { return Ok(c); }
    }
    Err("parallel-rustc-coordinator not found".into())
}

fn queue_file_path() -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    PathBuf::from(format!("/tmp/parallel-rustc-queue-{}{}.jsonl", std::process::id(), ts))
}

pub async fn run_build_v5(config: &BuildConfig) -> Result<BuildV5Summary, String> {
    let coordinator = locate_coordinator()?;
    let queue_file = queue_file_path();

    println!("parallel-rustc build (v5: RUSTC_WRAPPER metadata pipelining)");
    println!("manifest: {}", config.manifest_path.as_deref()
        .map(|p| p.display().to_string()).unwrap_or_else(|| "Cargo.toml".into()));
    println!("profile: {}   jobs/codegen-phase: {}",
        if config.release { "release" } else { "debug" }, config.jobs);
    println!("coordinator: {}", coordinator.display());
    println!("queue:       {}", queue_file.display());
    println!();

    let _ = std::fs::remove_file(&queue_file);
    let overall = Instant::now();

    println!("[1/3] cargo build (metadata-only forward pass)...");
    let cargo_started = Instant::now();
    run_cargo_with_coordinator(&coordinator, &queue_file, config).await?;
    let cargo_pass = cargo_started.elapsed();
    println!("      done in {:.2}s", cargo_pass.as_secs_f64());

    println!("[2/3] parsing queue and assigning codegen phases...");
    let units = parse_queue(&queue_file)?;
    if units.is_empty() {
        let total = overall.elapsed();
        println!("      no codegen queued (total: {:.2}s)", total.as_secs_f64());
        return Ok(BuildV5Summary {
            total, cargo_pass, codegen_pass: Duration::ZERO,
            queued_units: 0, phases: 0, max_phase_width: 0,
        });
    }
    let phases = assign_phases(&units);
    let max_width = phases.iter().map(|p| p.len()).max().unwrap_or(0);
    println!("      {} units   {} phases   max phase width = {}", units.len(), phases.len(), max_width);

    println!("[3/3] running full codegen in parallel...");
    let codegen_started = Instant::now();
    run_codegen_phases(&units, &phases, config).await?;
    let codegen_pass = codegen_started.elapsed();
    println!("      done in {:.2}s", codegen_pass.as_secs_f64());

    let total = overall.elapsed();
    println!();
    println!("total: {:.2}s   (cargo: {:.2}s   codegen: {:.2}s)",
        total.as_secs_f64(), cargo_pass.as_secs_f64(), codegen_pass.as_secs_f64());

    Ok(BuildV5Summary {
        total, cargo_pass, codegen_pass,
        queued_units: units.len(), phases: phases.len(), max_phase_width: max_width,
    })
}

async fn run_cargo_with_coordinator(
    coordinator: &Path,
    queue_file: &Path,
    config: &BuildConfig,
) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    if config.release { cmd.arg("--release"); }
    if let Some(p) = &config.manifest_path {
        cmd.arg("--manifest-path").arg(p);
    }
    cmd.env("RUSTC_WRAPPER", coordinator);
    cmd.env("PARALLEL_RUSTC_QUEUE", queue_file);
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());

    let status = cmd.status().await
        .map_err(|e| format!("spawn cargo build (v5): {e}"))?;
    if !status.success() {
        return Err(format!("cargo build (v5 forward pass) failed: {status}"));
    }
    Ok(())
}

fn parse_queue(queue_file: &Path) -> Result<Vec<RustcUnit>, String> {
    let contents = match std::fs::read_to_string(queue_file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read queue {}: {e}", queue_file.display())),
    };

    let mut units = Vec::new();
    for (i, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let v: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("queue line {}: {e}", i + 1))?;

        let cwd = v.get("cwd").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let env: Vec<(String, String)> = v.get("env").and_then(|x| x.as_array())
            .map(|items| items.iter().filter_map(|pair| {
                let p = pair.as_array()?;
                Some((p.first()?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string()))
            }).collect())
            .unwrap_or_default();
        let argv: Vec<String> = v.get("argv").and_then(|x| x.as_array())
            .map(|items| items.iter().filter_map(|s| s.as_str().map(|x| x.to_string())).collect())
            .unwrap_or_default();

        if argv.is_empty() { continue; }
        units.push(build_unit_from_argv(argv, cwd, env));
    }
    Ok(units)
}

fn build_unit_from_argv(argv: Vec<String>, cwd: String, env: Vec<(String, String)>) -> RustcUnit {
    let mut crate_name = String::new();
    let mut out_dir = String::new();
    let mut externs = Vec::new();
    let mut emit = String::new();
    let mut crate_type = String::new();
    let mut src = String::new();

    let mut it = argv.iter().skip(1).peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--crate-name" => { if let Some(v) = it.next() { crate_name = v.clone(); } }
            "--out-dir"    => { if let Some(v) = it.next() { out_dir = v.clone(); } }
            "--extern"     => { if let Some(v) = it.next() {
                if let Some(eq) = v.find('=') { externs.push(v[eq+1..].to_string()); }
            }}
            "--emit"       => { if let Some(v) = it.next() { emit = v.clone(); } }
            "--crate-type" => { if let Some(v) = it.next() { crate_type = v.clone(); } }
            s if s.starts_with("--crate-name=") => crate_name = s["--crate-name=".len()..].to_string(),
            s if s.starts_with("--out-dir=")    => out_dir    = s["--out-dir=".len()..].to_string(),
            s if s.starts_with("--extern=") => {
                let v = &s["--extern=".len()..];
                if let Some(eq) = v.find('=') { externs.push(v[eq+1..].to_string()); }
            }
            s if s.starts_with("--emit=")       => emit       = s["--emit=".len()..].to_string(),
            s if s.starts_with("--crate-type=") => crate_type = s["--crate-type=".len()..].to_string(),
            s if !s.starts_with('-') && src.is_empty() => src = s.to_string(),
            _ => {}
        }
    }
    RustcUnit { args: argv, cwd, env, crate_name, out_dir, externs, emit, crate_type, src }
}

async fn run_codegen_phases(
    units: &[RustcUnit],
    phases: &[Vec<usize>],
    config: &BuildConfig,
) -> Result<(), String> {
    let concurrency = config.jobs.max(1);
    for (pi, phase) in phases.iter().enumerate() {
        if phase.is_empty() { continue; }
        let runnable: Vec<usize> = phase.iter().copied()
            .filter(|&idx| !units[idx].should_skip_replay())
            .collect();
        if runnable.is_empty() { continue; }
        println!("  phase {} ({} unit{})...", pi, runnable.len(),
            if runnable.len() == 1 { "" } else { "s" });
        let phase_t = Instant::now();
        for wave in runnable.chunks(concurrency) {
            let mut set: JoinSet<Result<(), String>> = JoinSet::new();
            for &idx in wave {
                let unit = units[idx].clone();
                set.spawn(async move { run_codegen_unit(&unit).await });
            }
            while let Some(res) = set.join_next().await {
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => { set.abort_all(); return Err(e); }
                    Err(e) => { set.abort_all(); return Err(format!("join error: {e}")); }
                }
            }
        }
        println!("    done in {:.2}s", phase_t.elapsed().as_secs_f64());
    }
    Ok(())
}

async fn run_codegen_unit(unit: &RustcUnit) -> Result<(), String> {
    if unit.args.is_empty() {
        return Err("empty argv".into());
    }
    if !unit.out_dir.is_empty() {
        let _ = std::fs::create_dir_all(&unit.out_dir);
    }
    let mut cmd = Command::new(&unit.args[0]);
    cmd.args(&unit.args[1..]);
    if !unit.cwd.is_empty() { cmd.current_dir(&unit.cwd); }
    for (k, v) in &unit.env { cmd.env(k, v); }
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());

    let status = cmd.status().await
        .map_err(|e| format!("spawn rustc for {}: {e}", unit.label()))?;
    if !status.success() {
        return Err(format!("rustc failed for {} ({}): {status}", unit.label(), unit.crate_type));
    }
    Ok(())
}
