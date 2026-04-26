//! `parallel-rustc-coordinator` — RUSTC_WRAPPER for v0.5.0 metadata pipelining.
//!
//! Goal: let Cargo's forward pass advance to dependents as soon as `.rmeta`
//! is ready, while the producer's `.rlib` continues codegen in the background
//! in parallel with other crates' `.rmeta` work.
//!
//! ## Algorithm (per invocation)
//!
//! 1. Detect probes (`-vV`, `--print …`) and build-script binaries → exec
//!    rustc directly and exit.
//! 2. Otherwise, before launching rustc, wait until every `--extern name=PATH`
//!    file exists on disk. (Some upstream may still be doing codegen in the
//!    background from an earlier coordinator call.)
//! 3. Spawn rustc with the *original* args (full `--emit=…,metadata,link`).
//!    rustc writes `.rmeta` early in compilation, then continues to `.rlib`.
//! 4. Poll for the `.rmeta` to appear on disk. Once it's there, return
//!    success to Cargo immediately — the rustc child continues codegen in
//!    the background (we deliberately leak the `Child` handle).
//! 5. Append a record of the in-flight invocation (PID, expected outputs,
//!    argv) to the shared queue file under `flock`. The orchestrator will
//!    wait for all PIDs to terminate before reporting overall success.
//!
//! No metadata-rewrite is required: rustc's normal `--emit=…,metadata,link`
//! already produces `.rmeta` before `.rlib`. We just need to return early.
//!
//! ## Fallback paths
//!
//! - No `PARALLEL_RUSTC_QUEUE` env var → behave as plain passthrough.
//! - `--emit` doesn't include `metadata` → no early-return win possible,
//!   passthrough.
//! - rustc exits before `.rmeta` appears → it failed, propagate exit code.

use std::env;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const QUEUE_ENV: &str = "PARALLEL_RUSTC_QUEUE";
const RMETA_POLL_TIMEOUT: Duration = Duration::from_secs(120);
const EXTERN_WAIT_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

fn main() {
    let args: Vec<OsString> = env::args_os().collect();
    if args.len() < 2 {
        eprintln!("parallel-rustc-coordinator: missing rustc path");
        process::exit(2);
    }

    let rustc = args[1].clone();
    let rustc_args: Vec<OsString> = args[2..].to_vec();

    let rustc_args_str: Option<Vec<String>> = rustc_args
        .iter()
        .map(|s| s.to_str().map(|x| x.to_string()))
        .collect();
    let rustc_args_str = match rustc_args_str {
        Some(v) => v,
        None => exec_passthrough(&rustc, &rustc_args),
    };

    if should_passthrough(&rustc_args_str) {
        exec_passthrough(&rustc, &rustc_args);
    }

    // Wait for every --extern artifact to materialize. They may be produced
    // by background rustc children spawned by earlier coordinator calls.
    let externs = collect_extern_paths(&rustc_args_str);
    if let Err(e) = wait_for_paths(&externs, EXTERN_WAIT_TIMEOUT) {
        eprintln!("parallel-rustc-coordinator: {e}");
        process::exit(1);
    }

    // Spawn rustc with original args. rustc emits .rmeta early; we'll return
    // success to Cargo as soon as it appears, leaving codegen to finish in bg.
    let mut child = match Command::new(&rustc)
        .args(&rustc_args)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "parallel-rustc-coordinator: failed to spawn {}: {e}",
                rustc.to_string_lossy()
            );
            process::exit(1);
        }
    };
    let pid = child.id();

    // Compute expected .rmeta path from --crate-name + --out-dir + extra-filename.
    let rmeta_path = expected_rmeta_path(&rustc_args_str);

    // Poll for .rmeta or for rustc to exit (failure case).
    let started = Instant::now();
    loop {
        // Check rustc exit: if rustc died, propagate.
        match child.try_wait() {
            Ok(Some(status)) => {
                // rustc finished entirely. .rmeta may or may not exist.
                if status.success() {
                    // It finished fully; queue with empty pid (already done).
                    let _ = enqueue(pid, &rmeta_path, &rustc, &rustc_args_str, true);
                    process::exit(0);
                } else {
                    process::exit(status.code().unwrap_or(1));
                }
            }
            Ok(None) => { /* still running */ }
            Err(e) => {
                eprintln!("parallel-rustc-coordinator: try_wait failed: {e}");
                let _ = child.kill();
                process::exit(1);
            }
        }

        if let Some(p) = &rmeta_path {
            if p.exists() {
                break;
            }
        } else {
            // No predictable .rmeta path — fall back to waiting for full exit.
            // (Rare: shouldn't happen for normal lib/bin compiles.)
            match child.wait() {
                Ok(s) => process::exit(s.code().unwrap_or(1)),
                Err(e) => {
                    eprintln!("parallel-rustc-coordinator: wait failed: {e}");
                    process::exit(1);
                }
            }
        }

        if started.elapsed() > RMETA_POLL_TIMEOUT {
            eprintln!(
                "parallel-rustc-coordinator: timeout waiting for .rmeta at {:?}",
                rmeta_path
            );
            let _ = child.kill();
            process::exit(1);
        }
        thread::sleep(POLL_INTERVAL);
    }

    // .rmeta is on disk. Record the in-flight rustc PID for the orchestrator
    // to await after cargo's forward pass finishes, then exit success and
    // leak the Child so the rustc process keeps running.
    if let Err(e) = enqueue(pid, &rmeta_path, &rustc, &rustc_args_str, false) {
        // If we can't enqueue, fall back to waiting for rustc here so the
        // build still produces correct artifacts.
        eprintln!("parallel-rustc-coordinator: enqueue failed: {e}; waiting for rustc");
        match child.wait() {
            Ok(s) => process::exit(s.code().unwrap_or(1)),
            Err(_) => process::exit(1),
        }
    }

    // Detach: forget the Child so dropping it doesn't kill the process. (Rust's
    // std::process::Child does not kill on drop, but we forget to be explicit.)
    std::mem::forget(child);
    process::exit(0);
}

fn should_passthrough(args: &[String]) -> bool {
    if args.iter().any(|a| a == "-vV" || a == "-V" || a == "--version") {
        return true;
    }
    if args.iter().any(|a| a == "--print" || a.starts_with("--print=")) {
        return true;
    }
    let crate_name = find_value(args, "--crate-name");
    if let Some(name) = &crate_name {
        if name.starts_with("build_script_") {
            return true;
        }
    }
    if crate_name.is_none() {
        return true;
    }
    // No --emit, or emit doesn't include metadata — no early-return win.
    let emit = match find_value(args, "--emit") {
        Some(e) => e,
        None => return true,
    };
    if !emit.contains("metadata") {
        return true;
    }
    // If queue env is missing, we're not orchestrated — passthrough so the
    // build still works as a normal cargo build with our wrapper.
    if env::var(QUEUE_ENV).is_err() {
        return true;
    }
    false
}

fn find_value(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().cloned();
        }
        if let Some(rest) = a.strip_prefix(&prefix) {
            return Some(rest.to_string());
        }
    }
    None
}

fn collect_extern_paths(args: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let val = if a == "--extern" {
            it.next().cloned()
        } else if let Some(rest) = a.strip_prefix("--extern=") {
            Some(rest.to_string())
        } else {
            None
        };
        if let Some(v) = val {
            // value is `name=path` or just `name` (no path → builtin like proc_macro).
            if let Some(eq) = v.find('=') {
                let path = &v[eq + 1..];
                out.push(PathBuf::from(path));
            }
        }
    }
    out
}

fn wait_for_paths(paths: &[PathBuf], timeout: Duration) -> Result<(), String> {
    let started = Instant::now();
    loop {
        let missing: Vec<&PathBuf> = paths.iter().filter(|p| !p.exists()).collect();
        if missing.is_empty() {
            return Ok(());
        }
        if started.elapsed() > timeout {
            return Err(format!(
                "timeout waiting for {} extern path(s); first missing: {}",
                missing.len(),
                missing[0].display()
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Compute the path rustc will write the `.rmeta` to. Cargo always passes
/// `--out-dir` and a `-C extra-filename=-HASH` so the file is
/// `<out_dir>/lib<crate_name><extra_filename>.rmeta` (for lib crates;
/// bin crates use `<crate_name><extra_filename>` with no extension).
///
/// For our purposes any crate that emits metadata produces a .rmeta in this
/// canonical location.
fn expected_rmeta_path(args: &[String]) -> Option<PathBuf> {
    let crate_name = find_value(args, "--crate-name")?;
    let out_dir = find_value(args, "--out-dir")?;
    let extra = find_extra_filename(args).unwrap_or_default();
    // Build script `bin` crates would use no `lib` prefix, but we already
    // passthrough those; everything reaching here is rmeta-emitting.
    let crate_types: Vec<String> = collect_crate_types(args);
    let is_lib = crate_types.iter().any(|t| {
        t == "lib" || t == "rlib" || t == "dylib" || t == "proc-macro" || t == "cdylib" || t == "staticlib"
    });
    let file_name = if is_lib || crate_types.is_empty() {
        format!("lib{crate_name}{extra}.rmeta")
    } else {
        // bin crates: rustc still writes a .rmeta when emit contains metadata,
        // but uses no `lib` prefix.
        format!("{crate_name}{extra}.rmeta")
    };
    Some(PathBuf::from(out_dir).join(file_name))
}

fn find_extra_filename(args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let val = if a == "-C" {
            it.next().cloned()
        } else if let Some(rest) = a.strip_prefix("-C") {
            Some(rest.to_string())
        } else {
            None
        };
        if let Some(v) = val {
            if let Some(rest) = v.strip_prefix("extra-filename=") {
                return Some(rest.to_string());
            }
        }
    }
    None
}

fn collect_crate_types(args: &[String]) -> Vec<String> {
    let mut types = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let val = if a == "--crate-type" {
            it.next().cloned()
        } else if let Some(rest) = a.strip_prefix("--crate-type=") {
            Some(rest.to_string())
        } else {
            None
        };
        if let Some(v) = val {
            for t in v.split(',') {
                types.push(t.to_string());
            }
        }
    }
    types
}

/// Append a JSON record to the queue file (flock-serialized).
///
/// Record fields:
///   pid:      rustc PID still finishing in background (0 if already done)
///   rmeta:    path to .rmeta we waited for (may be null)
///   argv:     full rustc argv (for diagnostics / replay if PID dies)
///   already_done: true if rustc fully exited before we got here
fn enqueue(
    pid: u32,
    rmeta: &Option<PathBuf>,
    rustc: &OsString,
    rustc_args: &[String],
    already_done: bool,
) -> std::io::Result<()> {
    let queue = match env::var(QUEUE_ENV) {
        Ok(p) => p,
        Err(_) => return Ok(()), // no queue — silently skip
    };

    let mut argv: Vec<String> = Vec::with_capacity(1 + rustc_args.len());
    argv.push(rustc.to_string_lossy().into_owned());
    argv.extend(rustc_args.iter().cloned());

    let record = serde_json::json!({
        "pid": pid,
        "rmeta": rmeta.as_ref().map(|p| p.to_string_lossy().into_owned()),
        "already_done": already_done,
        "crate_name": find_value(rustc_args, "--crate-name").unwrap_or_default(),
        "argv": argv,
    });
    let line = serde_json::to_string(&record).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, format!("serialize: {e}"))
    })?;

    let f: File = OpenOptions::new().create(true).append(true).open(&queue)?;
    lock_exclusive(&f)?;
    let mut f = f;
    writeln!(f, "{line}")?;
    f.flush()?;
    Ok(())
}

#[cfg(unix)]
fn lock_exclusive(f: &File) -> std::io::Result<()> {
    let fd = f.as_raw_fd();
    let r = unsafe { libc_flock(fd, LOCK_EX) };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
const LOCK_EX: i32 = 2;

#[cfg(unix)]
extern "C" {
    #[link_name = "flock"]
    fn libc_flock(fd: i32, operation: i32) -> i32;
}

#[allow(dead_code)]
fn _path_unused(_p: &Path) {}

fn exec_passthrough(rustc: &OsString, rustc_args: &[OsString]) -> ! {
    let status = Command::new(rustc).args(rustc_args).status();
    match status {
        Ok(s) => process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!(
                "parallel-rustc-coordinator: failed to exec {}: {e}",
                rustc.to_string_lossy()
            );
            process::exit(1);
        }
    }
}
