//! `parallel-rustc-wrapper` — a pass-through `rustc` wrapper that records invocations.
//!
//! Cargo invokes `$RUSTC_WRAPPER rustc <args>`. This binary:
//! 1. Optionally appends a JSON-encoded record of the invocation (cwd, env
//!    overrides, argv) as a single line to the file named by
//!    `$PARALLEL_RUSTC_RECORD_FILE`.
//! 2. Execs the real rustc with the same args so the build proceeds normally.
//!
//! Each record is a JSON array:
//!   [cwd, [[env_key, env_val], ...], rustc_path, rustc_arg, rustc_arg, ...]
//!
//! That is: element 0 is cwd, element 1 is env overrides we care about, and
//! elements 2.. are the argv (index 0 = rustc path).
//!
//! We rely on O_APPEND atomicity under PIPE_BUF for interleave safety.

use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::process;

// Env vars cargo sets for rustc that matter for reproducing the invocation.
// We capture whatever is in the environment when the wrapper runs, since cargo
// sets these before exec.
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
    "DEP_PROTOBUF_INCLUDE", // DEP_* build-script-exported env is important
];

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        eprintln!("parallel-rustc-wrapper: missing rustc path (expected RUSTC_WRAPPER invocation)");
        process::exit(2);
    }

    let rustc = &args[1];
    let rustc_args = &args[2..];

    if let Ok(record_file) = env::var("PARALLEL_RUSTC_RECORD_FILE") {
        let cwd = env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();

        let mut envs: Vec<(String, String)> = Vec::new();
        // Capture whitelisted env vars plus any DEP_* / CARGO_CFG_* / CARGO_FEATURE_*.
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

        let mut entry: Vec<serde_json::Value> = Vec::with_capacity(2 + args.len());
        entry.push(serde_json::Value::String(cwd));
        entry.push(serde_json::to_value(&envs).unwrap_or(serde_json::Value::Array(vec![])));
        for a in &args[1..] {
            entry.push(serde_json::Value::String(a.clone()));
        }

        if let Ok(line) = serde_json::to_string(&entry) {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&record_file) {
                let _ = writeln!(f, "{line}");
            }
        }
    }

    let status = process::Command::new(rustc)
        .args(rustc_args)
        .status()
        .unwrap_or_else(|e| {
            eprintln!("parallel-rustc-wrapper: failed to exec {rustc}: {e}");
            process::exit(1);
        });

    process::exit(status.code().unwrap_or(1));
}
