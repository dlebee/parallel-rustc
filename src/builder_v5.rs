//! v0.5.0 — RUSTC_WRAPPER as build coordinator with metadata pipelining.
//!
//! Strategy:
//! 1. Set RUSTC_WRAPPER=parallel-rustc-coordinator and run `cargo build`.
//! 2. The coordinator returns success to Cargo as soon as `.rmeta` is on
//!    disk, leaving rustc to finish codegen in the background. It records
//!    each background rustc PID in a shared queue file.
//! 3. After Cargo finishes its forward pass, we read the queue and wait for
//!    every background rustc PID to terminate. If any failed, we fail the
//!    build.
//!
//! See spec/0.5.0.md.
//!
//! ## Why this beats default cargo build on small core counts
//!
//! Cargo's job server caps concurrent rustc processes to `-j` (default
//! `nproc`). On a 2-CPU machine this means at most 2 rustc processes. With
//! pipelining still active, those 2 slots are sometimes spent on codegen
//! while a downstream type-check is starved.
//!
//! Our coordinator returns success to Cargo as soon as a producer's `.rmeta`
//! lands. Cargo immediately spawns the dependent rustc — we now have a
//! consumer rustc *plus* the producer's still-running codegen rustc both
//! using CPU. We deliberately oversubscribe the OS scheduler beyond Cargo's
//! `-j`, trading throughput for occasionally idle slots.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::process::Command;

use crate::builder::BuildConfig;

#[derive(Debug, Clone)]
pub struct BuildV5Summary {
    pub total: Duration,
    pub cargo_pass: Duration,
    pub drain_pass: Duration,
    pub queued_units: usize,
    pub still_running_at_drain: usize,
}

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
        "could not locate parallel-rustc-coordinator binary; build it first with \
         `cargo build --release --bin parallel-rustc-coordinator` or set \
         PARALLEL_RUSTC_COORDINATOR_BIN"
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

#[derive(Debug, Clone)]
struct QueueEntry {
    pid: u32,
    crate_name: String,
    already_done: bool,
}

pub async fn run_build_v5(config: &BuildConfig) -> Result<BuildV5Summary, String> {
    let coordinator = locate_coordinator()?;
    let queue_file = queue_file_path();

    println!("parallel-rustc build (v5: RUSTC_WRAPPER metadata pipelining)");
    if let Some(p) = &config.manifest_path {
        println!("manifest: {}", p.display());
    }
    println!(
        "profile: {}",
        if config.release { "release" } else { "debug" }
    );
    println!("coordinator: {}", coordinator.display());
    println!("queue: {}", queue_file.display());
    println!();

    let _ = std::fs::remove_file(&queue_file);

    let overall = Instant::now();

    println!("[1/2] cargo build (early-return on .rmeta via coordinator)...");
    let cargo_started = Instant::now();
    run_cargo_with_coordinator(&coordinator, &queue_file, config).await?;
    let cargo_pass = cargo_started.elapsed();
    println!("      cargo forward pass done in {:.2}s", cargo_pass.as_secs_f64());

    println!("[2/2] draining background codegen...");
    let drain_started = Instant::now();
    let entries = parse_queue(&queue_file)?;
    let still_running = entries.iter().filter(|e| !e.already_done && e.pid != 0).count();
    println!(
        "      {} queued unit(s), {} still running at drain start",
        entries.len(),
        still_running
    );
    drain_background(&entries).await?;
    let drain_pass = drain_started.elapsed();
    println!("      drain done in {:.2}s", drain_pass.as_secs_f64());

    let total = overall.elapsed();
    println!();
    println!(
        "total: {:.2}s   (cargo: {:.2}s   drain: {:.2}s)",
        total.as_secs_f64(),
        cargo_pass.as_secs_f64(),
        drain_pass.as_secs_f64()
    );

    Ok(BuildV5Summary {
        total,
        cargo_pass,
        drain_pass,
        queued_units: entries.len(),
        still_running_at_drain: still_running,
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
    cmd.env("RUSTC_WRAPPER", coordinator);
    cmd.env("PARALLEL_RUSTC_QUEUE", queue_file);
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());

    let status = cmd
        .status()
        .await
        .map_err(|e| format!("failed to spawn cargo build (v5): {e}"))?;
    if !status.success() {
        return Err(format!("cargo build (v5) failed with {status}"));
    }
    Ok(())
}

fn parse_queue(queue_file: &Path) -> Result<Vec<QueueEntry>, String> {
    let contents = match std::fs::read_to_string(queue_file) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read queue {}: {e}", queue_file.display())),
    };
    let mut out = Vec::new();
    for (i, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("queue line {}: parse JSON: {e}", i + 1))?;
        let pid = v.get("pid").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        let crate_name = v
            .get("crate_name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let already_done = v.get("already_done").and_then(|x| x.as_bool()).unwrap_or(false);
        out.push(QueueEntry { pid, crate_name, already_done });
    }
    Ok(out)
}

/// Wait for each queued background rustc PID to terminate. Polls /proc/<pid>
/// since we are not the parent (the rustc child was reparented to init when
/// the coordinator process exited).
async fn drain_background(entries: &[QueueEntry]) -> Result<(), String> {
    use tokio::time::sleep;
    let poll = Duration::from_millis(50);

    let still_pending: Vec<&QueueEntry> = entries
        .iter()
        .filter(|e| !e.already_done && e.pid != 0)
        .collect();

    if still_pending.is_empty() {
        return Ok(());
    }

    // Poll all PIDs concurrently — but they all just need /proc lookups.
    let timeout = Duration::from_secs(900);
    let started = Instant::now();
    let mut remaining: Vec<&QueueEntry> = still_pending;
    while !remaining.is_empty() {
        remaining.retain(|e| pid_alive(e.pid));
        if remaining.is_empty() {
            break;
        }
        if started.elapsed() > timeout {
            let names: Vec<String> = remaining
                .iter()
                .map(|e| format!("{}({})", e.crate_name, e.pid))
                .collect();
            return Err(format!(
                "timeout waiting for background rustc pids: {}",
                names.join(", ")
            ));
        }
        sleep(poll).await;
    }
    Ok(())
}

/// On Linux, /proc/<pid> exists iff the process exists. We can't waitid()
/// because the child was reparented to init.
fn pid_alive(pid: u32) -> bool {
    let p = format!("/proc/{pid}");
    std::path::Path::new(&p).exists()
}
