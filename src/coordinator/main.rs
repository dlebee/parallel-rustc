//! `parallel-rustc-coordinator` — RUSTC_WRAPPER for v0.5.0 metadata pipelining.
//!
//! Implements the spec/0.5.0.md approach:
//!
//! - Probes (`-vV`, `--print …`) and build-script binaries: exec rustc directly.
//! - Lib crates (--emit contains metadata AND link):
//!   1. Rewrite `--emit` → `dep-info,metadata` (fast, produces .rmeta only)
//!   2. Run rustc → .rmeta lands on disk
//!   3. Queue ORIGINAL args + captured env → orchestrator runs full codegen later
//!   4. Return success to Cargo
//! - Crates with link but no metadata (bins/proc-macro wrapper entries):
//!   1. Return success to Cargo WITHOUT running rustc
//!   2. Queue original args + env for orchestrator's codegen pass
//!
//! ## Queue format (one JSON per line)
//!
//!   [ cwd, [[env_k, env_v], ...], rustc_path, rustc_arg, ... ]
//!
//! This matches the format produced by `parallel-rustc-wrapper` so we can
//! reuse `recorder::parse_recorded` / `assign_phases` in the orchestrator.
//!
//! ## Concurrency safety
//!
//! Multiple Cargo workers call us concurrently. Queue writes are serialised
//! with `flock(LOCK_EX)`.

use std::env;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::process::{self, Command};

const QUEUE_ENV: &str = "PARALLEL_RUSTC_QUEUE";

const CAPTURED_ENV: &[&str] = &[
    "CARGO",
    "CARGO_MANIFEST_DIR",
    "CARGO_PKG_NAME",
    "CARGO_PKG_VERSION",
    "CARGO_PKG_AUTHORS",
    "CARGO_PKG_DESCRIPTION",
    "CARGO_PKG_HOMEPAGE",
    "CARGO_PKG_REPOSITORY",
    "CARGO_PKG_LICENSE",
    "CARGO_PKG_LICENSE_FILE",
    "CARGO_PKG_VERSION_MAJOR",
    "CARGO_PKG_VERSION_MINOR",
    "CARGO_PKG_VERSION_PATCH",
    "CARGO_PKG_VERSION_PRE",
    "CARGO_PKG_RUST_VERSION",
    "CARGO_PKG_README",
    "CARGO_CRATE_NAME",
    "CARGO_BIN_NAME",
    "CARGO_PRIMARY_PACKAGE",
    "CARGO_TARGET_TMPDIR",
    "OUT_DIR",
    "LD_LIBRARY_PATH",
    "DYLD_LIBRARY_PATH",
    "DYLD_FALLBACK_LIBRARY_PATH",
    "RUSTC_BOOTSTRAP",
    "RUSTC",
    "RUSTC_WORKSPACE_WRAPPER",
    "TARGET",
    "HOST",
    "PROFILE",
    "OPT_LEVEL",
    "DEBUG",
    "NUM_JOBS",
    "RUSTFLAGS",
    "CARGO_ENCODED_RUSTFLAGS",
];

fn main() {
    let args: Vec<OsString> = env::args_os().collect();
    if args.len() < 2 {
        eprintln!("parallel-rustc-coordinator: missing rustc path");
        process::exit(2);
    }

    let rustc = args[1].clone();
    let rustc_args: Vec<OsString> = args[2..].to_vec();

    // Convert to Strings for inspection. Non-UTF-8 → passthrough.
    let rustc_args_str: Option<Vec<String>> = rustc_args
        .iter()
        .map(|s| s.to_str().map(|x| x.to_string()))
        .collect();
    let rustc_args_str = match rustc_args_str {
        Some(v) => v,
        None => exec_passthrough(&rustc, &rustc_args),
    };

    // No queue → plain pass-through (safe fallback, works as a no-op wrapper).
    if env::var(QUEUE_ENV).is_err() {
        exec_passthrough(&rustc, &rustc_args);
    }

    if should_passthrough(&rustc_args_str) {
        exec_passthrough(&rustc, &rustc_args);
    }

    let emit = find_value(&rustc_args_str, "--emit").unwrap_or_default();
    let has_metadata = emit.contains("metadata");
    let has_link = emit.contains("link");

    if has_metadata {
        // Lib crate: run metadata-only, queue full args.
        let meta_args = rewrite_emit_metadata(&rustc_args_str);
        let status = Command::new(&rustc).args(&meta_args).status();
        let code = match status {
            Ok(s) => s.code().unwrap_or(1),
            Err(e) => {
                eprintln!("parallel-rustc-coordinator: exec failed: {e}");
                1
            }
        };
        if code != 0 {
            process::exit(code);
        }
        if let Err(e) = enqueue(&rustc, &rustc_args_str) {
            eprintln!("parallel-rustc-coordinator: enqueue failed: {e}");
            process::exit(1);
        }
        process::exit(0);
    } else if has_link {
        // Bin/dylib crate: suppress compile, queue for later full run.
        if let Err(e) = enqueue(&rustc, &rustc_args_str) {
            eprintln!("parallel-rustc-coordinator: enqueue (bin) failed: {e}");
            // Fall back to running normally so the build still works.
            exec_passthrough(&rustc, &rustc_args);
        }
        process::exit(0);
    } else {
        exec_passthrough(&rustc, &rustc_args);
    }
}

fn should_passthrough(args: &[String]) -> bool {
    if args.iter().any(|a| a == "-vV" || a == "-V" || a == "--version") {
        return true;
    }
    if args.iter().any(|a| a == "--print" || a.starts_with("--print=")) {
        return true;
    }
    if let Some(name) = find_value(args, "--crate-name") {
        if name.starts_with("build_script_") {
            return true;
        }
    }
    // No --crate-name → not a real compile invocation.
    if find_value(args, "--crate-name").is_none() {
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

fn rewrite_emit_metadata(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--emit" && i + 1 < args.len() {
            out.push("--emit=dep-info,metadata".to_string());
            i += 2;
            continue;
        }
        if a.starts_with("--emit=") {
            out.push("--emit=dep-info,metadata".to_string());
            i += 1;
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

/// Append a record to the queue file using the same format as
/// `parallel-rustc-wrapper`:
///   [ cwd, [[env_k, env_v], ...], rustc_path, rustc_arg, ... ]
///
/// This lets the orchestrator reuse `recorder::parse_recorded` and
/// `recorder::assign_phases`.
fn enqueue(rustc: &OsString, rustc_args: &[String]) -> std::io::Result<()> {
    let queue = env::var(QUEUE_ENV).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::Other, "PARALLEL_RUSTC_QUEUE not set")
    })?;

    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Capture whitelisted env vars plus DEP_* / CARGO_CFG_* / CARGO_FEATURE_*.
    let mut envs: Vec<(String, String)> = Vec::new();
    for key in CAPTURED_ENV {
        if let Ok(v) = env::var(key) {
            envs.push(((*key).to_string(), v));
        }
    }
    for (k, v) in env::vars() {
        if k.starts_with("DEP_")
            || k.starts_with("CARGO_CFG_")
            || k.starts_with("CARGO_FEATURE_")
            || k.starts_with("CARGO_BIN_EXE_")
        {
            envs.push((k, v));
        }
    }

    // Build the JSON array: [cwd, [[k,v],...], rustc, arg, arg, ...]
    let mut entry: Vec<serde_json::Value> = Vec::with_capacity(3 + rustc_args.len());
    entry.push(serde_json::Value::String(cwd));
    entry.push(serde_json::to_value(&envs).unwrap_or(serde_json::Value::Array(vec![])));
    entry.push(serde_json::Value::String(
        rustc.to_string_lossy().into_owned(),
    ));
    for a in rustc_args {
        entry.push(serde_json::Value::String(a.clone()));
    }

    let line = serde_json::to_string(&entry).map_err(|e| {
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
