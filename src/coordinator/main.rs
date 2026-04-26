//! `parallel-rustc-coordinator` — RUSTC_WRAPPER for v0.5.0 metadata pipelining.
//!
//! For each lib crate rustc invocation:
//! 1. Rewrite --emit → dep-info,metadata (fast .rmeta only)
//! 2. Run rustc synchronously → Cargo gets .rmeta and advances
//! 3. Write original argv + cwd + env to queue file for later parallel replay
//!
//! Probes, build scripts, and build-script deps pass straight through.
//!
//! Queue format: one JSON object per line
//!   {"cwd": "...", "env": [[k,v],...], "argv": ["rustc", ...]}

use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::process::{self, Command};
use std::sync::OnceLock;

const QUEUE_ENV: &str = "PARALLEL_RUSTC_QUEUE";
const NEEDS_RLIB_ENV: &str = "PARALLEL_RUSTC_NEEDS_RLIB";

/// Crate names (underscore-normalized) the v5 builder pre-classified as
/// needing a full `.rlib` during cargo's forward pass. Read from
/// `PARALLEL_RUSTC_NEEDS_RLIB` once on first use.
fn needs_rlib_set() -> &'static HashSet<String> {
    static CELL: OnceLock<HashSet<String>> = OnceLock::new();
    CELL.get_or_init(|| {
        env::var(NEEDS_RLIB_ENV)
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect()
    })
}

const CAPTURED_ENV: &[&str] = &[
    "CARGO", "CARGO_MANIFEST_DIR", "CARGO_PKG_NAME", "CARGO_PKG_VERSION",
    "CARGO_CRATE_NAME", "CARGO_BIN_NAME", "CARGO_PRIMARY_PACKAGE",
    "CARGO_TARGET_TMPDIR", "OUT_DIR", "LD_LIBRARY_PATH",
    "RUSTC_BOOTSTRAP", "RUSTC", "TARGET", "HOST", "PROFILE",
    "OPT_LEVEL", "DEBUG", "NUM_JOBS", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS",
];

fn main() {
    let args: Vec<OsString> = env::args_os().collect();
    if args.len() < 2 {
        eprintln!("parallel-rustc-coordinator: missing rustc path");
        process::exit(2);
    }

    let rustc = &args[1];
    let rustc_args: Vec<OsString> = args[2..].to_vec();

    let rustc_args_str: Option<Vec<String>> = rustc_args
        .iter()
        .map(|s| s.to_str().map(|x| x.to_string()))
        .collect();
    let rustc_args_str = match rustc_args_str {
        Some(v) => v,
        None => exec_passthrough(rustc, &rustc_args),
    };

    if env::var(QUEUE_ENV).is_err() {
        exec_passthrough(rustc, &rustc_args);
    }

    if should_passthrough(&rustc_args_str) {
        exec_passthrough(rustc, &rustc_args);
    }

    let emit = find_value(&rustc_args_str, "--emit").unwrap_or_default();
    let crate_type = find_value(&rustc_args_str, "--crate-type").unwrap_or_default();
    let is_bin = crate_type == "bin" || crate_type.contains("bin");

    if !emit.contains("metadata") && !is_bin {
        // Not a lib (no metadata) and not a bin we want to defer → passthrough.
        exec_passthrough(rustc, &rustc_args);
    }

    if is_bin {
        // Bin link step: defer entirely — just queue it for after codegen replay.
        // Don't run rustc at all; produce a fake empty output so Cargo thinks
        // it succeeded. Cargo will check for the binary in --out-dir.
        // We'll run the real link after codegen replay when all .rlib are ready.
        if let Err(e) = enqueue(rustc, &rustc_args_str) {
            eprintln!("coordinator: enqueue (bin) failed: {e}");
            // Fall back: actually run it (will fail if deps missing)
            exec_passthrough(rustc, &rustc_args);
        }
        // Write a stub binary so Cargo's post-build checks pass.
        if let Some(out_dir) = find_value(&rustc_args_str, "--out-dir") {
            if let Some(name) = find_value(&rustc_args_str, "--crate-name") {
                let stub = format!("{out_dir}/{name}");
                let _ = std::fs::write(&stub, b"");
                // Make it executable
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&stub,
                        std::fs::Permissions::from_mode(0o755));
                }
            }
        }
        process::exit(0);
    }

    // Metadata-only pass (synchronous — Cargo waits for .rmeta).
    let meta_args = rewrite_emit_metadata(&rustc_args_str);
    let code = match Command::new(rustc).args(&meta_args).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => { eprintln!("coordinator: metadata pass: {e}"); 1 }
    };
    if code != 0 { process::exit(code); }

    // Queue original args for parallel replay after Cargo finishes.
    if let Err(e) = enqueue(rustc, &rustc_args_str) {
        eprintln!("coordinator: enqueue failed: {e}");
        process::exit(1);
    }

    process::exit(0);
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
        // v0.5.1: pre-classified needs-rlib set wins. These crates are
        // (transitive) dependencies of build scripts or proc-macros and will
        // be linked during cargo's forward pass — they need a real .rlib.
        if needs_rlib_set().contains(&name) {
            return true;
        }
    }
    // Build script DEPENDENCIES land in target/{profile}/build/ — they need
    // full .rlib because the build script binary links them.
    if let Some(out_dir) = find_value(args, "--out-dir") {
        if out_dir.contains("/build/") {
            return true;
        }
    }
    if find_value(args, "--crate-name").is_none() {
        return true;
    }
    if find_value(args, "--emit").is_none() {
        return true;
    }
    false
}

fn find_value(args: &[String], flag: &str) -> Option<String> {
    let prefix = format!("{flag}=");
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag { return it.next().cloned(); }
        if let Some(rest) = a.strip_prefix(&prefix) { return Some(rest.to_string()); }
    }
    None
}

fn rewrite_emit_metadata(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--emit" {
            out.push("--emit=dep-info,metadata".to_string());
            i += 2;
        } else if a.starts_with("--emit=") {
            out.push("--emit=dep-info,metadata".to_string());
            i += 1;
        } else {
            out.push(a.clone());
            i += 1;
        }
    }
    out
}

fn enqueue(rustc: &OsString, rustc_args: &[String]) -> std::io::Result<()> {
    let queue = env::var(QUEUE_ENV).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::Other, format!("QUEUE_ENV: {e}"))
    })?;

    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    // Capture ALL env vars — rustc/cargo rely on many env!() macros at
    // compile time (CARGO_PKG_VERSION_MINOR, RUST_HOST_TARGET, etc.) that
    // are set by Cargo for the specific invocation. Capturing everything
    // ensures the codegen replay has the same env context as the forward pass.
    let envs: Vec<(String, String)> = env::vars().collect();

    let mut argv: Vec<String> = Vec::with_capacity(1 + rustc_args.len());
    argv.push(rustc.to_string_lossy().into_owned());
    argv.extend(rustc_args.iter().cloned());

    let record = serde_json::json!({"cwd": cwd, "env": envs, "argv": argv});
    let line = serde_json::to_string(&record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;

    let f: File = OpenOptions::new().create(true).append(true).open(&queue)?;
    lock_exclusive(&f)?;
    let mut f = f;
    writeln!(f, "{line}")?;
    f.flush()
}

#[cfg(unix)]
fn lock_exclusive(f: &File) -> std::io::Result<()> {
    let fd = f.as_raw_fd();
    let r = unsafe { libc_flock(fd, LOCK_EX) };
    if r != 0 { return Err(std::io::Error::last_os_error()); }
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
    match Command::new(rustc).args(rustc_args).status() {
        Ok(s) => process::exit(s.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("coordinator: exec failed: {e}");
            process::exit(1);
        }
    }
}
