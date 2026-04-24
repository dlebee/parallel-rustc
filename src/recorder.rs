//! Recorder: parse the rustc invocation log produced by
//! `parallel-rustc-wrapper` and turn it into a phase-ordered execution plan.
//!
//! Record-file line format (one JSON array per line):
//!   [ cwd, [[env_k, env_v], ...], rustc_path, rustc_arg, rustc_arg, ... ]
//!
//! Phase assignment uses Kahn's algorithm on the DAG induced by
//! `--extern name=<path>` edges. Cargo emits artifacts with disambiguated
//! filenames like `libfoo-<hash>.rlib` into a shared `deps/` directory; we
//! match a consumer's extern to its producer by (out_dir, crate_name).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

/// A single recorded rustc invocation.
#[derive(Debug, Clone)]
pub struct RustcUnit {
    /// Full argv including rustc path at index 0. Ready to feed to `Command`.
    pub args: Vec<String>,
    /// Working directory Cargo used when invoking rustc. Required for replay
    /// because arg paths like `src/lib.rs` are relative.
    pub cwd: String,
    /// Env vars Cargo set for this invocation that affect the build.
    pub env: Vec<(String, String)>,
    /// Crate name (from `--crate-name`). Empty if absent (build scripts sometimes).
    pub crate_name: String,
    /// Output directory (from `--out-dir`).
    pub out_dir: String,
    /// Extern paths (values from `--extern name=<path>`).
    pub externs: Vec<String>,
    /// Emit kinds (from `--emit`). Useful for debugging / filtering.
    pub emit: String,
    /// Crate types (from `--crate-type`).
    pub crate_type: String,
    /// Source file (first non-flag positional argument).
    pub src: String,
}

impl RustcUnit {
    /// A short human label for logs.
    pub fn label(&self) -> String {
        if self.crate_name.is_empty() {
            if let Some(base) = Path::new(&self.src).file_name() {
                base.to_string_lossy().into_owned()
            } else {
                "<unknown>".to_string()
            }
        } else if self.crate_type.is_empty() {
            self.crate_name.clone()
        } else {
            format!("{} ({})", self.crate_name, self.crate_type)
        }
    }
}

/// Parse the record file into a list of units, in the order they were recorded.
pub fn parse_recorded(record_file: &Path) -> Result<Vec<RustcUnit>, String> {
    let contents = fs::read_to_string(record_file)
        .map_err(|e| format!("read record file {}: {e}", record_file.display()))?;

    let mut units = Vec::new();
    for (i, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|e| format!("line {}: parse JSON: {e}", i + 1))?;
        let arr = match value.as_array() {
            Some(a) => a,
            None => return Err(format!("line {}: expected JSON array", i + 1)),
        };
        if arr.len() < 3 {
            continue;
        }
        let cwd = arr[0].as_str().unwrap_or_default().to_string();
        let env: Vec<(String, String)> = arr[1]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|pair| {
                        let p = pair.as_array()?;
                        Some((
                            p.first()?.as_str()?.to_string(),
                            p.get(1)?.as_str()?.to_string(),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let argv: Vec<String> = arr[2..]
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        if argv.is_empty() {
            continue;
        }
        units.push(build_unit(argv, cwd, env));
    }
    Ok(units)
}

fn build_unit(argv: Vec<String>, cwd: String, env: Vec<(String, String)>) -> RustcUnit {
    let mut crate_name = String::new();
    let mut out_dir = String::new();
    let mut externs = Vec::new();
    let mut emit = String::new();
    let mut crate_type = String::new();
    let mut src = String::new();

    let mut it = argv.iter().skip(1).peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--crate-name" => {
                if let Some(v) = it.next() {
                    crate_name = v.clone();
                }
            }
            "--out-dir" => {
                if let Some(v) = it.next() {
                    out_dir = v.clone();
                }
            }
            "--extern" => {
                if let Some(v) = it.next() {
                    if let Some(eq) = v.find('=') {
                        externs.push(v[eq + 1..].to_string());
                    }
                }
            }
            "--emit" => {
                if let Some(v) = it.next() {
                    emit = v.clone();
                }
            }
            "--crate-type" => {
                if let Some(v) = it.next() {
                    crate_type = v.clone();
                }
            }
            s if s.starts_with("--crate-name=") => {
                crate_name = s["--crate-name=".len()..].to_string();
            }
            s if s.starts_with("--out-dir=") => {
                out_dir = s["--out-dir=".len()..].to_string();
            }
            s if s.starts_with("--extern=") => {
                let v = &s["--extern=".len()..];
                if let Some(eq) = v.find('=') {
                    externs.push(v[eq + 1..].to_string());
                }
            }
            s if s.starts_with("--emit=") => {
                emit = s["--emit=".len()..].to_string();
            }
            s if s.starts_with("--crate-type=") => {
                crate_type = s["--crate-type=".len()..].to_string();
            }
            s if !s.starts_with('-') && src.is_empty() => {
                src = s.to_string();
            }
            _ => {}
        }
    }

    RustcUnit {
        args: argv,
        cwd,
        env,
        crate_name,
        out_dir,
        externs,
        emit,
        crate_type,
        src,
    }
}

/// Build adjacency info and assign each unit to a phase via Kahn's algorithm.
///
/// Returns `phases`: `phases[p]` is the list of unit indices in phase `p`.
/// Units with no deps land in phase 0; every edge goes to a strictly later phase.
///
/// Edge model: a unit U depends on unit V iff some `--extern name=<path>`
/// of U points at an artifact V emits. We match (parent_of_extern_path,
/// crate_name_derived_from_filename) against (out_dir, crate_name) of
/// potential producers. Producers recorded after the consumer in the serial
/// record are ignored (can't be a true dep since the pass was serial).
pub fn assign_phases(units: &[RustcUnit]) -> Vec<Vec<usize>> {
    let n = units.len();
    if n == 0 {
        return Vec::new();
    }

    let mut producers: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, u) in units.iter().enumerate() {
        if u.crate_name.is_empty() || u.out_dir.is_empty() {
            continue;
        }
        producers
            .entry((normalize(&u.out_dir), u.crate_name.clone()))
            .or_default()
            .push(i);
    }

    let mut in_deg = vec![0usize; n];
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut deps: Vec<HashSet<usize>> = vec![HashSet::new(); n];

    for (i, u) in units.iter().enumerate() {
        for ext_path in &u.externs {
            let path = Path::new(ext_path);
            let parent = path
                .parent()
                .map(|p| normalize(&p.to_string_lossy()))
                .unwrap_or_default();
            let file_name = match path.file_name().map(|s| s.to_string_lossy().into_owned()) {
                Some(f) => f,
                None => continue,
            };
            let crate_name = match extract_crate_name(&file_name) {
                Some(n) => n,
                None => continue,
            };
            let key = (parent, crate_name);
            if let Some(prods) = producers.get(&key) {
                let mut chosen: Option<usize> = None;
                for &p in prods {
                    if p < i {
                        chosen = Some(match chosen {
                            Some(c) if c > p => c,
                            _ => p,
                        });
                    }
                }
                if let Some(p) = chosen {
                    if deps[i].insert(p) {
                        adj[p].push(i);
                        in_deg[i] += 1;
                    }
                }
            }
        }
    }

    let mut phase_of = vec![0usize; n];
    let mut remaining = in_deg.clone();
    let mut current: Vec<usize> = (0..n).filter(|&i| remaining[i] == 0).collect();
    current.sort_unstable();
    let mut p = 0usize;
    let mut processed = 0usize;

    while !current.is_empty() {
        let mut next = Vec::new();
        for &u in &current {
            phase_of[u] = p;
            processed += 1;
            for &v in &adj[u] {
                remaining[v] -= 1;
                if remaining[v] == 0 {
                    next.push(v);
                }
            }
        }
        next.sort_unstable();
        current = next;
        p += 1;
    }

    if processed < n {
        for i in 0..n {
            if remaining[i] > 0 {
                phase_of[i] = p;
            }
        }
        p += 1;
    }

    let mut phases: Vec<Vec<usize>> = vec![Vec::new(); p];
    for (i, &ph) in phase_of.iter().enumerate() {
        phases[ph].push(i);
    }
    phases
}

fn normalize(p: &str) -> String {
    p.trim_end_matches('/').to_string()
}

/// Given an artifact file name like `libfoo-abc123.rlib` or `foo-abc123`,
/// return the crate name (matching `--crate-name`, which uses `_` for `-`).
fn extract_crate_name(file_name: &str) -> Option<String> {
    let stem = file_name.strip_prefix("lib").unwrap_or(file_name);
    let stem = match stem.find('.') {
        Some(i) => &stem[..i],
        None => stem,
    };
    let name = match stem.rsplit_once('-') {
        Some((n, _hash)) if !n.is_empty() => n,
        _ => stem,
    };
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_invocation() {
        let argv = vec![
            "/usr/bin/rustc".to_string(),
            "--crate-name".to_string(),
            "foo".to_string(),
            "--crate-type".to_string(),
            "lib".to_string(),
            "--out-dir".to_string(),
            "/tmp/target/deps".to_string(),
            "--emit".to_string(),
            "dep-info,metadata,link".to_string(),
            "src/lib.rs".to_string(),
        ];
        let u = build_unit(argv, "/tmp".to_string(), vec![]);
        assert_eq!(u.crate_name, "foo");
        assert_eq!(u.out_dir, "/tmp/target/deps");
        assert_eq!(u.crate_type, "lib");
        assert_eq!(u.src, "src/lib.rs");
        assert_eq!(u.cwd, "/tmp");
    }

    #[test]
    fn extract_names() {
        assert_eq!(extract_crate_name("libfoo-abc123def456.rlib").as_deref(), Some("foo"));
        assert_eq!(extract_crate_name("libfoo_bar-abc.rmeta").as_deref(), Some("foo_bar"));
        assert_eq!(extract_crate_name("foo-abc.so").as_deref(), Some("foo"));
    }

    fn u(name: &str, out: &str, externs: &[&str]) -> RustcUnit {
        RustcUnit {
            args: vec![],
            cwd: "/t".into(),
            env: vec![],
            crate_name: name.into(),
            out_dir: out.into(),
            externs: externs.iter().map(|s| s.to_string()).collect(),
            emit: "link".into(),
            crate_type: "lib".into(),
            src: format!("{name}.rs"),
        }
    }

    #[test]
    fn extern_edges_form_phases() {
        let foo = u("foo", "/t/deps", &[]);
        let bar = u("bar", "/t/deps", &[]);
        let baz = u(
            "baz",
            "/t/deps",
            &["/t/deps/libfoo-abc123.rlib", "/t/deps/libbar-def456.rlib"],
        );
        let phases = assign_phases(&[foo, bar, baz]);
        assert_eq!(phases.len(), 2, "phases: {phases:?}");
        assert!(phases[0].contains(&0));
        assert!(phases[0].contains(&1));
        assert_eq!(phases[1], vec![2]);
    }

    #[test]
    fn unrelated_siblings_share_phase() {
        let a = u("a", "/t/deps", &[]);
        let b = u("b", "/t/deps", &[]);
        let phases = assign_phases(&[a, b]);
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0].len(), 2);
    }

    #[test]
    fn empty_input_gives_empty_phases() {
        let phases = assign_phases(&[]);
        assert!(phases.is_empty());
    }
}
