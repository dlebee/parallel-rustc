//! v0.4.0 — Unit-graph driven parallel executor.
//!
//! Strategy: for each DAG phase, run one `cargo build -p <pkg>` per
//! package in parallel. Each invocation gets its own isolated
//! `CARGO_TARGET_DIR` that is *seeded* (via hardlinks) from a shared
//! "merged" target dir containing all artifacts produced so far.
//! After every phase the new artifacts are merged back in.
//!
//! This avoids Cargo's per-target-dir file lock entirely — no two
//! concurrent cargo processes share a target dir.
//!
//! See `spec/0.4.0.md` for design rationale.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;
use tokio::task::JoinSet;

use crate::builder::BuildConfig;
use crate::unit_graph::{self, UnitGraphUnit};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Summary returned to callers (bench harness, integration tests).
#[derive(Debug, Clone)]
pub struct BuildV4Summary {
    /// Total wall-clock from the first line of output to the last.
    pub total: Duration,
    /// Number of DAG phases.
    pub phases: usize,
    /// Widest phase (most packages compiled in parallel in any one wave).
    pub max_phase_width: usize,
    /// Total unit-graph units (all modes).
    pub units: usize,
    /// Packages we actually drove with `cargo build -p`.
    pub driven_units: usize,
    /// Wall-clock inside the compile-phase loop only.
    pub compile: Duration,
}

/// Entry point for `parallel-rustc build-v4`.
pub async fn run_build_v4(config: &BuildConfig) -> Result<BuildV4Summary, String> {
    let profile_name = if config.release { "release" } else { "debug" };

    println!("parallel-rustc build (v4: unit-graph parallel)");
    if let Some(p) = &config.manifest_path {
        println!("manifest:  {}", p.display());
    }
    println!("profile:   {}   jobs/phase: {}", profile_name, config.jobs);
    println!();

    let overall = Instant::now();

    // ------------------------------------------------------------------
    // Step 1: fetch unit graph
    // ------------------------------------------------------------------
    println!("[1/4] fetching unit graph (cargo +nightly)…");
    let t = Instant::now();
    let units = unit_graph::fetch_unit_graph(
        config.manifest_path.as_deref(),
        config.release,
    )
    .map_err(|e| format!("unit-graph: {e}"))?;
    let phases = unit_graph::assign_phases(&units)
        .map_err(|e| format!("phase computation: {e}"))?;
    let max_width = phases.iter().map(|p| p.len()).max().unwrap_or(0);
    println!(
        "      {} units  {} phases  max width {}  ({:.2}s)",
        units.len(),
        phases.len(),
        max_width,
        t.elapsed().as_secs_f64()
    );

    // ------------------------------------------------------------------
    // Step 2: prepare merged + iso root dirs (fresh each run)
    // ------------------------------------------------------------------
    let ws = workspace_dir(config.manifest_path.as_deref());
    let v4_root  = ws.join("target-v4");
    let merged   = v4_root.join("merged");
    let iso_root = v4_root.join("iso");

    // Always start cold so phase N+1 only sees phase N's artifacts.
    let _ = fs::remove_dir_all(&v4_root);
    for sub in &["deps", ".fingerprint", "build"] {
        fs::create_dir_all(merged.join(profile_name).join(sub))
            .map_err(|e| format!("mkdir merged/{sub}: {e}"))?;
    }
    fs::create_dir_all(&iso_root)
        .map_err(|e| format!("mkdir iso/: {e}"))?;

    println!("[2/4] target dirs:  {}", v4_root.display());

    // ------------------------------------------------------------------
    // Step 3: phase-driven parallel compile
    // ------------------------------------------------------------------
    println!("[3/4] parallel compile…");
    let compile_t = Instant::now();
    let mut driven_units = 0usize;

    if config.batched {
        // Batched mode: one cargo invocation per phase (all pkgs in the phase as
        // -p flags), respecting dependency order across phases.
        // This avoids Cargo's feature resolver panic that occurs when mixing
        // packages from different dependency levels in a single invocation.
        for (pi, phase_idxs) in phases.iter().enumerate() {
            let mut pkgs: Vec<&UnitGraphUnit> = Vec::new();
            let mut seen_pkg: std::collections::HashSet<&str> =
                std::collections::HashSet::new();
            for &idx in phase_idxs {
                let u = &units[idx];
                if u.mode == "build" && seen_pkg.insert(u.pkg_id.as_str()) {
                    pkgs.push(u);
                }
            }
            if pkgs.is_empty() { continue; }
            println!(
                "  phase {} batched: {} pkgs, -j{}",
                pi, pkgs.len(), config.jobs
            );
            let ph_t = Instant::now();
            // Batched: use the normal target dir (no isolation needed
            // since we run one cargo invocation at a time per phase).
            run_phase_batched(&pkgs, config).await?;
            driven_units += pkgs.len();
            println!("    done in {:.2}s", ph_t.elapsed().as_secs_f64());
        }
    } else {
    for (pi, phase_idxs) in phases.iter().enumerate() {
        // Collect one representative unit per pkg_id, mode == "build".
        // `run-custom-build` units are handled implicitly by whichever
        // `cargo build -p` owns that package.
        let mut pkgs: Vec<&UnitGraphUnit> = Vec::new();
        let mut seen_pkg: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for &idx in phase_idxs {
            let u = &units[idx];
            if u.mode == "build" && seen_pkg.insert(u.pkg_id.as_str()) {
                pkgs.push(u);
            }
        }
        if pkgs.is_empty() {
            continue; // pure build-script-only phase — skip
        }

        let ph_t = Instant::now();
        println!(
            "  phase {pi} ({} pkg{})…",
            pkgs.len(),
            if pkgs.len() == 1 { "" } else { "s" }
        );

        // Seed all iso dirs before launching (avoids concurrent reads
        // of merged/ racing with writes from prior phase merge).
        for u in &pkgs {
            let iso = iso_path(&iso_root, u);
            seed_iso(&iso, &merged, profile_name)?;
        }

        // Launch in parallel, bounded by config.jobs.
        run_phase_wave(&pkgs, &iso_root, profile_name, config).await?;

        // Merge new artifacts back into merged/.
        for u in &pkgs {
            let iso = iso_path(&iso_root, u);
            merge_into(
                &iso.join(profile_name),
                &merged.join(profile_name),
            )?;
        }

        driven_units += pkgs.len();
        println!("    done in {:.2}s", ph_t.elapsed().as_secs_f64());
    }
    }  // end non-batched branch
    let compile = compile_t.elapsed();

    // ------------------------------------------------------------------
    // Step 4: final link into the real workspace target/
    // ------------------------------------------------------------------
    println!(
        "[4/4] final link into {}/target …",
        ws.display()
    );
    let link_t = Instant::now();
    let real_target = ws.join("target");
    fs::create_dir_all(real_target.join(profile_name))
        .map_err(|e| format!("mkdir real target: {e}"))?;

    // Copy merged artifacts into the real target so cargo can find them.
    hardlink_tree(
        &merged.join(profile_name),
        &real_target.join(profile_name),
    )?;

    if !config.batched {
        // One final `cargo build` to handle any remaining linking / binaries.
        // With the target dir pre-warmed this should finish in < 1 s.
        // In batched mode we already built every package in one cargo
        // invocation — the real target is fully warm, so this step adds
        // only overhead.
        let mut cmd = Command::new("cargo");
        cmd.arg("build");
        if config.release {
            cmd.arg("--release");
        }
        if let Some(p) = &config.manifest_path {
            cmd.arg("--manifest-path").arg(p);
        }
        cmd.env("CARGO_TARGET_DIR", &real_target);
        cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        let status = cmd.status().await
            .map_err(|e| format!("final cargo build: {e}"))?;
        if !status.success() {
            return Err(format!("final cargo build failed: {status}"));
        }
    }
    println!("      done in {:.2}s", link_t.elapsed().as_secs_f64());

    let total = overall.elapsed();
    println!();
    println!(
        "total: {:.2}s  (compile phases: {:.2}s)",
        total.as_secs_f64(),
        compile.as_secs_f64()
    );

    Ok(BuildV4Summary {
        total,
        phases: phases.len(),
        max_phase_width: max_width,
        units: units.len(),
        driven_units,
        compile,
    })
}

// ---------------------------------------------------------------------------
// Phase execution
// ---------------------------------------------------------------------------

async fn run_phase_batched(
    pkgs: &[&UnitGraphUnit],
    config: &BuildConfig,
) -> Result<(), String> {
    // Single cargo invocation for the whole phase, passing every package
    // with its own `-p`. Cargo handles parallelism internally via `-j`.
    // Uses the normal target dir (no isolation) since batched phases are
    // sequential — no lock contention.
    let mut cmd = Command::new("cargo");
    cmd.arg("build");
    for u in pkgs {
        cmd.arg("-p").arg(pkg_spec(u));
    }
    if config.release {
        cmd.arg("--release");
    }
    cmd.arg("-j").arg(config.jobs.to_string());
    if let Some(p) = &config.manifest_path {
        cmd.arg("--manifest-path").arg(p);
    }
    // Suppress stdout; keep stderr visible so errors surface.
    cmd.stdout(Stdio::null()).stderr(Stdio::inherit());

    let status = cmd.status().await
        .map_err(|e| format!("spawn cargo build (batched phase): {e}"))?;
    if !status.success() {
        return Err(format!(
            "cargo build (batched phase, {} pkgs) failed: {status}",
            pkgs.len()
        ));
    }
    Ok(())
}

async fn run_phase_wave(
    pkgs: &[&UnitGraphUnit],
    iso_root: &Path,
    _profile_name: &str,
    config: &BuildConfig,
) -> Result<(), String> {
    let concurrency = config.jobs.max(1);
    for chunk in pkgs.chunks(concurrency) {
        let mut set: JoinSet<Result<(), String>> = JoinSet::new();
        for u in chunk {
            let iso = iso_path(iso_root, u);
            let spec = pkg_spec(u);
            let manifest = config.manifest_path.clone();
            let release = config.release;
            set.spawn(async move {
                cargo_build_isolated(&spec, &iso, manifest.as_deref(), release).await
            });
        }
        while let Some(res) = set.join_next().await {
            match res {
                Ok(Ok(())) => {}
                Ok(Err(e)) => { set.abort_all(); return Err(e); }
                Err(e)     => { set.abort_all(); return Err(format!("join error: {e}")); }
            }
        }
    }
    Ok(())
}

async fn cargo_build_isolated(
    pkg: &str,
    iso_dir: &Path,
    manifest_path: Option<&Path>,
    release: bool,
) -> Result<(), String> {
    let mut cmd = Command::new("cargo");
    cmd.arg("build").arg("-p").arg(pkg);
    if release { cmd.arg("--release"); }
    // -j1 inside each process: parallelism is across processes, not within.
    cmd.arg("-j").arg("1");
    if let Some(p) = manifest_path {
        cmd.arg("--manifest-path").arg(p);
    }
    cmd.env("CARGO_TARGET_DIR", iso_dir);
    // Suppress stdout/stderr to avoid interleaved noise from N concurrent builds.
    // Errors still propagate as Err(String).
    cmd.stdout(Stdio::null()).stderr(Stdio::null());

    let status = cmd.status().await
        .map_err(|e| format!("spawn cargo build -p {pkg}: {e}"))?;
    if !status.success() {
        // Re-run with stderr visible so the user sees the actual error.
        let mut cmd2 = Command::new("cargo");
        cmd2.arg("build").arg("-p").arg(pkg);
        if release { cmd2.arg("--release"); }
        cmd2.arg("-j").arg("1");
        if let Some(p) = manifest_path {
            cmd2.arg("--manifest-path").arg(p);
        }
        cmd2.env("CARGO_TARGET_DIR", iso_dir);
        cmd2.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        let _ = cmd2.status().await;
        return Err(format!("cargo build -p {pkg} failed ({status})"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Seed: hardlink merged/* into iso dir before compiling
// ---------------------------------------------------------------------------

fn seed_iso(iso_dir: &Path, merged: &Path, profile_name: &str) -> Result<(), String> {
    let iso_p  = iso_dir.join(profile_name);
    let mrg_p  = merged.join(profile_name);
    for sub in &["deps", ".fingerprint", "build"] {
        fs::create_dir_all(iso_p.join(sub))
            .map_err(|e| format!("mkdir iso/{sub}: {e}"))?;
        hardlink_tree(&mrg_p.join(sub), &iso_p.join(sub))?;
    }
    Ok(())
}

/// Recursively hardlink every regular file from `src` into `dst`.
/// Existing files are skipped (same name ⇒ same content due to cargo's
/// hash-keyed filenames).
fn hardlink_tree(src: &Path, dst: &Path) -> Result<(), String> {
    if !src.exists() { return Ok(()); }
    if !src.is_dir() { return link_one(src, dst); }
    fs::create_dir_all(dst)
        .map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    for entry in fs::read_dir(src)
        .map_err(|e| format!("readdir {}: {e}", src.display()))?
    {
        let entry = entry.map_err(|e| format!("readdir entry: {e}"))?;
        let ft = entry.file_type().map_err(|e| format!("file_type: {e}"))?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if ft.is_dir() {
            hardlink_tree(&s, &d)?;
        } else if ft.is_file() {
            link_one(&s, &d)?;
        } else if ft.is_symlink() {
            if !d.exists() {
                if let Ok(target) = fs::read_link(&s) {
                    #[cfg(unix)]
                    { let _ = std::os::unix::fs::symlink(&target, &d); }
                    #[cfg(not(unix))]
                    { let _ = fs::copy(&s, &d); }
                }
            }
        }
    }
    Ok(())
}

fn link_one(src: &Path, dst: &Path) -> Result<(), String> {
    if dst.exists() { return Ok(()); }
    match fs::hard_link(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(_) => fs::copy(src, dst)
            .map(|_| ())
            .map_err(|e| format!("link/copy {} -> {}: {e}", src.display(), dst.display())),
    }
}

// ---------------------------------------------------------------------------
// Merge: pull new artifacts from iso back into merged
// ---------------------------------------------------------------------------

fn merge_into(iso_profile: &Path, merged_profile: &Path) -> Result<(), String> {
    for sub in &["deps", ".fingerprint", "build"] {
        let s = iso_profile.join(sub);
        let d = merged_profile.join(sub);
        if s.exists() {
            fs::create_dir_all(&d)
                .map_err(|e| format!("mkdir merged/{sub}: {e}"))?;
            merge_tree(&s, &d)?;
        }
    }
    Ok(())
}

fn merge_tree(src: &Path, dst: &Path) -> Result<(), String> {
    for entry in fs::read_dir(src)
        .map_err(|e| format!("readdir {}: {e}", src.display()))?
    {
        let entry = entry.map_err(|e| format!("readdir entry: {e}"))?;
        let ft = entry.file_type().map_err(|e| format!("file_type: {e}"))?;
        let s = entry.path();
        let d = dst.join(entry.file_name());
        if ft.is_dir() {
            fs::create_dir_all(&d)
                .map_err(|e| format!("mkdir {}: {e}", d.display()))?;
            merge_tree(&s, &d)?;
        } else if ft.is_file() && !d.exists() {
            link_one(&s, &d)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// Package spec for `cargo -p`, including version to disambiguate
/// when multiple versions of the same crate are in the graph.
fn pkg_spec(u: &UnitGraphUnit) -> String {
    match &u.pkg_version {
        Some(v) => format!("{}@{}", u.pkg_name, v),
        None => u.pkg_name.clone(),
    }
}

fn workspace_dir(manifest_path: Option<&Path>) -> PathBuf {
    manifest_path
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        })
}

/// Stable, short directory name for an isolated target dir.
fn iso_path(iso_root: &Path, unit: &UnitGraphUnit) -> PathBuf {
    let key = fnv32(unit.pkg_id.as_bytes());
    iso_root.join(format!("{}-{key:08x}", sanitize(&unit.pkg_name)))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect()
}

fn fnv32(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for &b in data {
        h ^= b as u32;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn fnv32_stable() {
        assert_eq!(fnv32(b"abc"), fnv32(b"abc"));
        assert_ne!(fnv32(b"abc"), fnv32(b"abd"));
    }

    #[test]
    fn sanitize_converts_special() {
        assert_eq!(sanitize("hello-world"), "hello_world");
        assert_eq!(sanitize("proc_macro2"), "proc_macro2");
    }

    #[test]
    fn hardlink_tree_creates_files() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        fs::write(src.join("sub").join("b.txt"), b"world").unwrap();
        hardlink_tree(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(
            fs::read_to_string(dst.join("sub").join("b.txt")).unwrap(),
            "world"
        );
    }

    #[test]
    fn merge_tree_skips_existing() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("x.txt"), b"new").unwrap();
        fs::write(dst.join("x.txt"), b"old").unwrap();
        merge_tree(&src, &dst).unwrap();
        // Existing file must not be overwritten.
        assert_eq!(fs::read_to_string(dst.join("x.txt")).unwrap(), "old");
    }
}
