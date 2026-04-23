//! Invoke `cargo metadata` and parse the JSON into the structs we care about.
//!
//! We deliberately parse only the fields we need; unknown fields are ignored.
//! The schema is documented at <https://doc.rust-lang.org/cargo/commands/cargo-metadata.html>.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

/// Subset of `cargo metadata --format-version 1` output we consume.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Metadata {
    pub packages: Vec<Package>,
    pub workspace_members: Vec<String>,
    pub resolve: Resolve,
    #[serde(default)]
    pub workspace_root: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct Resolve {
    pub nodes: Vec<ResolveNode>,
    #[serde(default)]
    pub root: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ResolveNode {
    pub id: String,
    /// Package IDs this node depends on. `cargo metadata` also emits a
    /// richer `deps` field (with dep_kinds), but `dependencies` is the
    /// flat id list and is enough for v0 — we don't distinguish dev/build
    /// edges yet (see research/topics.md).
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub features: Vec<String>,
}

/// Run `cargo metadata --format-version 1` and parse its stdout.
pub fn load(manifest_path: Option<&Path>) -> Result<Metadata, String> {
    let mut cmd = Command::new("cargo");
    cmd.arg("metadata").arg("--format-version").arg("1");
    if let Some(p) = manifest_path {
        cmd.arg("--manifest-path").arg(p);
    }
    let out = cmd
        .output()
        .map_err(|e| format!("failed to spawn cargo: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "cargo metadata exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    serde_json::from_slice::<Metadata>(&out.stdout)
        .map_err(|e| format!("failed to parse cargo metadata JSON: {e}"))
}
