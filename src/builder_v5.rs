//! v0.5.x — RUSTC_WRAPPER with metadata-first pipelining + DAG-driven codegen replay.
//!
//! 1. Pre-classify via unit-graph: build-script/proc-macro deps → passthrough,
//!    pure libs → defer (metadata-only in forward pass, full codegen later).
//! 2. cargo build with coordinator: emits .rmeta only for deferred crates,
//!    queues original argv + env.
//! 3. DAG executor: fires each queued unit as soon as ALL its deps complete,
//!    bounded by config.jobs concurrent rustc processes. No phase barriers.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::builder::BuildConfig;
use crate::classifier::classify_from_unit_graph;
use crate::recorder::RustcUnit;

#[derive(Debug, Clone)]
pub struct BuildV5Summary {
    pub total: Duration,
    pub cargo_pass: Duration,
    pub codegen_pass: Duration,
    pub queued_units: usize,
    pub phases: usize,
    pub max_phase_width: usize,
}

pub fn locate_coordinator() -> Result<std::path::PathBuf, String> {
    if let Ok(p) = std::env::var("PARALLEL_RUSTC_COORDINATOR_BIN") {
        let pb = std::path::PathBuf::from(p);
        if pb.exists() { return Ok(pb); }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let c = dir.join("parallel-rustc-coordinator");
            if c.exists() { return Ok(c); }
        }
    }
    for sub in &["target/release", "target/debug"] {
        let c = std::path::PathBuf::from(sub).join("parallel-rustc-coordinator");
        if c.exists() { return Ok(c); }
    }
    Err("parallel-rustc-coordinator not found".into())
}

fn queue_file_path() -> std::path::PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    std::path::PathBuf::from(format!("/tmp/parallel-rustc-queue-{}{}.jsonl", std::process::id(), ts))
}

pub async fn run_build_v5(config: &BuildConfig) -> Result<BuildV5Summary, String> {
    let coordinator = locate_coordinator()?;
    let queue_file = queue_file_path();

    println!("parallel-rustc build (v5: RUSTC_WRAPPER metadata pipelining)");
    println!("manifest: {}", config.manifest_path.as_deref()
        .map(|p| p.display().to_string()).unwrap_or_else(|| "Cargo.toml".into()));
    println!("profile: {}   jobs: {}", if config.release { "release" } else { "debug" }, config.jobs);
    println!("coordinator: {}", coordinator.display());
    println!("queue:       {}", queue_file.display());
    println!();

    let _ = std::fs::remove_file(&queue_file);
    let overall = Instant::now();

    // Step 0: classify
    println!("[0/3] pre-classifying unit-graph (cargo +nightly --unit-graph)...");
    let classify_t = Instant::now();
    let classified = classify_from_unit_graph(
        config.manifest_path.as_deref(),
        config.release,
    ).unwrap_or_else(|e| {
        eprintln!("      classifier failed ({e}), falling back to full passthrough");
        crate::classifier::ClassifiedUnits {
            needs_rlib: HashSet::new(),
            defer_codegen: HashSet::new(),
            total_units: 0,
        }
    });
    let needs_rlib_csv = classified.needs_rlib.iter().cloned().collect::<Vec<_>>().join(",");
    println!("      pre-classification: {} passthrough, {} deferred ({} units total) in {:.2}s",
        classified.needs_rlib.len(), classified.defer_codegen.len(),
        classified.total_units, classify_t.elapsed().as_secs_f64());

    // Step 1: cargo forward pass (metadata-only for deferred crates)
    println!("[1/3] cargo build (metadata-only forward pass)...");
    let cargo_started = Instant::now();
    run_cargo_with_coordinator(&coordinator, &queue_file, &needs_rlib_csv, config).await?;
    let cargo_pass = cargo_started.elapsed();
    println!("      done in {:.2}s", cargo_pass.as_secs_f64());

    // Step 2: parse queue
    println!("[2/3] parsing queue...");
    let units = parse_queue(&queue_file)?;
    if units.is_empty() {
        let total = overall.elapsed();
        println!("      no codegen queued (total: {:.2}s)", total.as_secs_f64());
        return Ok(BuildV5Summary {
            total, cargo_pass, codegen_pass: Duration::ZERO,
            queued_units: 0, phases: 0, max_phase_width: 0,
        });
    }
    println!("      {} queued units", units.len());

    // Step 3: DAG-driven parallel codegen
    println!("[3/3] DAG codegen ({} max parallel)...", config.jobs.max(1));
    let codegen_started = Instant::now();
    run_codegen_dag(&units, config).await?;
    let codegen_pass = codegen_started.elapsed();
    println!("      done in {:.2}s", codegen_pass.as_secs_f64());

    let total = overall.elapsed();
    println!();
    println!("total: {:.2}s   (cargo: {:.2}s   codegen: {:.2}s)",
        total.as_secs_f64(), cargo_pass.as_secs_f64(), codegen_pass.as_secs_f64());

    Ok(BuildV5Summary {
        total, cargo_pass, codegen_pass,
        queued_units: units.len(), phases: 1, max_phase_width: units.len(),
    })
}

async fn run_cargo_with_coordinator(
    coordinator: &Path,
    queue_file: &Path,
    needs_rlib_csv: &str,
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
    cmd.env("PARALLEL_RUSTC_NEEDS_RLIB", needs_rlib_csv);
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
            }).collect()).unwrap_or_default();
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

/// DAG executor: fires each unit as soon as all its deps finish.
/// Bounded by `config.jobs` simultaneous rustc processes.
async fn run_codegen_dag(
    units: &[RustcUnit],
    config: &BuildConfig,
) -> Result<(), String> {
    let n = units.len();
    if n == 0 { return Ok(()); }

    // Build producers map: (normalized_out_dir, crate_name) → [unit_idx]
    let mut producers: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, u) in units.iter().enumerate() {
        if u.crate_name.is_empty() || u.out_dir.is_empty() { continue; }
        producers.entry((norm(&u.out_dir), u.crate_name.clone())).or_default().push(i);
    }

    // Build adj (dependency edges) and in_deg
    let mut in_deg = vec![0usize; n];
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];

    for (i, u) in units.iter().enumerate() {
        let mut seen: HashSet<usize> = HashSet::new();
        for ext_path in &u.externs {
            let p = Path::new(ext_path);
            let parent = p.parent()
                .map(|pp| norm(&pp.to_string_lossy()))
                .unwrap_or_default();
            let fname = match p.file_name().map(|s| s.to_string_lossy().into_owned()) {
                Some(f) => f, None => continue,
            };
            let cname = match extract_crate_name(&fname) {
                Some(c) => c, None => continue,
            };
            if let Some(prods) = producers.get(&(parent, cname)) {
                for &p in prods {
                    if p != i && seen.insert(p) {
                        adj[p].push(i);
                        in_deg[i] += 1;
                    }
                }
            }
        }
    }

    let runnable: Vec<bool> = (0..n).map(|i| !units[i].should_skip_replay()).collect();

    // For non-runnable units, treat as immediately done: decrement their dependents
    {
        let mut extra_done: VecDeque<usize> = (0..n)
            .filter(|&i| !runnable[i]).collect();
        while let Some(i) = extra_done.pop_front() {
            for &dep in &adj[i] {
                in_deg[dep] = in_deg[dep].saturating_sub(1);
            }
        }
    }

    let total_runnable = runnable.iter().filter(|&&r| r).count();
    let sem = Arc::new(Semaphore::new(config.jobs.max(1)));
    let in_deg = Arc::new(Mutex::new(in_deg));
    let adj = Arc::new(adj);
    let ready: Arc<Mutex<VecDeque<usize>>> = Arc::new(Mutex::new(
        (0..n).filter(|&i| runnable[i] && in_deg.lock().unwrap()[i] == 0).collect()
    ));
    let done_count = Arc::new(Mutex::new(0usize));

    let mut set: JoinSet<Result<(), String>> = JoinSet::new();

    loop {
        // Fire all currently-ready units
        loop {
            let idx = { ready.lock().unwrap().pop_front() };
            let idx = match idx { Some(i) => i, None => break };

            let sem2 = Arc::clone(&sem);
            let unit = units[idx].clone();
            let in_deg2 = Arc::clone(&in_deg);
            let adj2 = Arc::clone(&adj);
            let ready2 = Arc::clone(&ready);
            let done2 = Arc::clone(&done_count);

            set.spawn(async move {
                let _permit = sem2.acquire_owned().await.unwrap();
                let result = run_codegen_unit(&unit).await;
                if result.is_ok() {
                    let mut deg = in_deg2.lock().unwrap();
                    let mut q = ready2.lock().unwrap();
                    for &dep in &adj2[idx] {
                        deg[dep] = deg[dep].saturating_sub(1);
                        if deg[dep] == 0 {
                            q.push_back(dep);
                        }
                    }
                    *done2.lock().unwrap() += 1;
                }
                result
            });
        }

        if set.is_empty() { break; }

        // Wait for one task to complete, then loop to fire newly-ready units
        match set.join_next().await {
            Some(Ok(Err(e))) => { set.abort_all(); return Err(e); }
            Some(Err(e)) => { set.abort_all(); return Err(format!("join: {e}")); }
            _ => {}
        }
    }

    // Drain
    while let Some(res) = set.join_next().await {
        if let Ok(Err(e)) = res { return Err(e); }
    }

    let done = *done_count.lock().unwrap();
    if done < total_runnable {
        return Err(format!(
            "DAG executor: {done}/{total_runnable} completed (dependency cycle?)"
        ));
    }
    Ok(())
}

async fn run_codegen_unit(unit: &RustcUnit) -> Result<(), String> {
    if unit.args.is_empty() { return Err("empty argv".into()); }
    if !unit.out_dir.is_empty() { let _ = std::fs::create_dir_all(&unit.out_dir); }

    // Filter stale jobserver FDs from the captured env
    let env: Vec<(&str, &str)> = unit.env.iter()
        .filter(|(k, _)| k != "CARGO_MAKEFLAGS" && k != "MAKEFLAGS")
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    let mut cmd = Command::new(&unit.args[0]);
    cmd.args(&unit.args[1..]);
    if !unit.cwd.is_empty() { cmd.current_dir(&unit.cwd); }
    for (k, v) in &env { cmd.env(k, v); }
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());

    let status = cmd.status().await
        .map_err(|e| format!("spawn rustc for {}: {e}", unit.label()))?;
    if !status.success() {
        return Err(format!("rustc failed for {}: {status}", unit.label()));
    }
    Ok(())
}

fn norm(s: &str) -> String {
    s.trim_end_matches('/').to_string()
}

fn extract_crate_name(filename: &str) -> Option<String> {
    let stem = filename.strip_prefix("lib")?;
    let without_ext = stem.strip_suffix(".rlib")
        .or_else(|| stem.strip_suffix(".rmeta"))
        .or_else(|| stem.strip_suffix(".so"))
        .or_else(|| stem.strip_suffix(".d"))?;
    if let Some(pos) = without_ext.rfind('-') {
        let maybe_hash = &without_ext[pos + 1..];
        if maybe_hash.len() >= 8 && maybe_hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(without_ext[..pos].to_string());
        }
    }
    Some(without_ext.to_string())
}
