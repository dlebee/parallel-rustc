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
    /// Flat list of all dependency IDs (includes dev and build deps — use `deps` instead).
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// Rich dependency list with kind info. We use this to filter out dev-dependencies
    /// which would otherwise create spurious edges (and cycles in packages like serde
    /// that use themselves as a dev dep).
    #[serde(default)]
    pub deps: Vec<DepRef>,
    #[serde(default)]
    pub features: Vec<String>,
}

impl ResolveNode {
    /// Returns only the normal (non-dev, non-build-script) compile dependency IDs.
    /// Dev deps are excluded because they only apply to tests/benchmarks and including
    /// them creates false edges (and cycles) in the build graph.
    pub fn compile_deps(&self) -> Vec<&str> {
        if self.deps.is_empty() {
            // Fallback for old metadata format without `deps` field
            return self.dependencies.iter().map(|s| s.as_str()).collect();
        }
        self.deps
            .iter()
            .filter(|d| {
                // Keep only deps with at least one non-dev, non-build kind
                d.dep_kinds.iter().any(|k| {
                    k.kind.as_deref() != Some("dev")
                })
            })
            .map(|d| d.pkg.as_str())
            .collect()
    }
}

/// A resolved dependency reference with kind information.
#[derive(Debug, Deserialize)]
pub struct DepRef {
    /// Package ID of the dependency.
    pub pkg: String,
    /// Crate name as it appears in `extern crate`.
    #[allow(dead_code)]
    pub name: String,
    /// Dependency kinds (normal, dev, build).
    #[serde(default)]
    pub dep_kinds: Vec<DepKind>,
}

/// A single dependency kind entry.
#[derive(Debug, Deserialize)]
pub struct DepKind {
    /// `null` = normal, `"dev"` = dev dependency, `"build"` = build script dep.
    pub kind: Option<String>,
    /// Target platform filter, if any.
    #[allow(dead_code)]
    #[serde(default)]
    pub target: Option<String>,
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
