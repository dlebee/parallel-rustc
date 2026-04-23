# parallel-rustc

> ⚠️ **Proof of concept / experiment** — not production-ready.

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
