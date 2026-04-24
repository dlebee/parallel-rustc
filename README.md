# parallel-rustc

> ⚠️ **Proof of concept / experiment** — not production-ready. **Use at your own risk.**

A dependency-graph-aware parallel compilation planner for Rust workspaces.

## What is this?

Rust compilation is slow. `cargo build` parallelizes independent crates, but it's conservative — it won't start a dependent crate until all its dependencies have fully finished. This tool explores whether we can do better.

`parallel-rustc` parses `Cargo.lock` and workspace metadata to build the full dependency DAG, then computes **wavefront phases** — groups of crates that can be compiled simultaneously because all their dependencies are already satisfied.

```
$ parallel-rustc plan --manifest-path ./Cargo.toml

parallel-rustc plan
workspace: /home/user/my-project
packages: 16   phases: 5

phase 0 (6 packages, parallel):
  - chain-1 v0.1.0
  - leaf-a v0.1.0
  - leaf-b v0.1.0
  - leaf-c v0.1.0
  - leaf-d v0.1.0
  - leaf-e v0.1.0
phase 1 (6 packages, parallel):
  - chain-2 v0.1.0
  - diamond-left v0.1.0
  - diamond-right v0.1.0
  - shared-user-1 v0.1.0
  - shared-user-2 v0.1.0
  - shared-user-3 v0.1.0
phase 2 (2 packages, parallel):
  - chain-3 v0.1.0
  - diamond-top v0.1.0
phase 3 (1 packages, parallel):
  - chain-4 v0.1.0
phase 4 (1 packages, parallel):
  - app v0.1.0
```

## Status

**v0.0.0** — plan only. The tool computes and displays the compilation phases but does not invoke `rustc` yet. Think of it as `cargo build --dry-run` with better parallelism analysis.

### What works
- Parses `cargo metadata` (stable API, no nightly required)
- Builds a full dependency DAG using petgraph
- Computes wavefront phases via Kahn's algorithm
- Handles feature flags, optional dependencies, feature unification
- 15 integration tests covering: wide fan-out, linear chains, diamonds, shared deps, mixed topologies, feature propagation, multi-binary workspaces, and stress tests

### What's next (maybe)
- Actually invoke `rustc` per phase (v0.1.0)
- `.rmeta`-based early dependent start
- Critical-path prioritization
- Build script and proc-macro handling

## Install

```bash
cargo install --path .
```

## Usage

```bash
# Plan compilation phases for a workspace
parallel-rustc plan

# Specify a manifest path
parallel-rustc plan --manifest-path /path/to/Cargo.toml
```

## Why not just `cargo build -j N`?

Cargo's `-j` flag controls the number of concurrent rustc processes, but doesn't change dependency ordering. Cargo waits for a crate to fully finish before starting dependents. `parallel-rustc` explores:

- Starting dependent crates as soon as `.rmeta` (metadata) is available
- More aggressive batching across the dependency graph
- Critical-path prioritization to minimize wall-clock time

## Research

See [`research/topics.md`](research/topics.md) for notes on feature unification, build scripts, proc-macros, and the challenges of parallel Rust compilation.

## License

[MIT](LICENSE)

---

## 🚀 Quick Start (Try It Locally)

### 1. Install

```bash
# Clone and build
git clone https://github.com/dlebee-agent/parallel-rustc
cd parallel-rustc
cargo install --path .
```

### 2. Run on your own project

```bash
# Show the full dependency phase plan
parallel-rustc plan

# Or point at a specific workspace
parallel-rustc plan --manifest-path /path/to/your/project/Cargo.toml

# Workspace members only (hides external deps)
parallel-rustc plan --workspace-only
```

### 3. Example output

```
parallel-rustc plan
workspace: /home/you/my-project
packages: 34   phases: 9

phase 0 (14 packages, parallel):
  - cfg-if v1.0.0
  - unicode-ident v1.0.12
  - ...
phase 1 (8 packages, parallel):
  - proc-macro2 v1.0.86
  - quote v1.0.37
  - ...
phase 8 (1 packages, parallel):
  - my-project v0.1.0
```

### 4. What to look for

- **phases** — minimum number of sequential compilation steps (the "depth" of your dep graph)
- **Phase 0 width** — how many crates can compile in parallel immediately (wider = more initial parallelism)
- **Theoretical max parallelism** = `total packages / phases` — how much speedup is possible with infinite CPUs

### 5. Try it on a big project

```bash
git clone --depth 1 https://github.com/serde-rs/serde
parallel-rustc plan --manifest-path serde/Cargo.toml

git clone --depth 1 https://github.com/tokio-rs/tokio
parallel-rustc plan --manifest-path tokio/Cargo.toml --workspace-only
```
