# Research Topics

This document captures the two hardest correctness problems any parallel
`rustc` driver must solve before it can replace Cargo for incremental compilation:

1. Feature flags and feature unification.
2. The multiple target types inside a Cargo workspace.

Both problems ultimately boil down to the same insight: **the unit of compilation
is not "a crate" — it is a `(package, target, feature_set, compile_kind)` tuple.**
Getting this wrong produces either wrong binaries or redundant work.

---

## Topic 1: Feature flags in parallel compilation

### 1.1 Why features matter

Features are conditional compilation toggles (`#[cfg(feature = "foo")]`). Enabling
a feature can:

- Pull in additional optional dependencies.
- Activate features on existing dependencies (`features = ["x/y"]`).
- Change the public API (adding items, methods, impls).
- Change what symbols end up in `rlib`s.

Therefore **two compilations of the same crate with different feature sets
produce different `rlib`s** and cannot be substituted for each other.

### 1.2 Feature unification

Cargo performs a workspace-wide resolution called "feature unification". For any
crate X reachable in the dependency graph, Cargo computes the **union** of all
features requested along every path from the workspace root(s) to X. All
consumers of X then see a single build of X with that unified feature set.

Example: if workspace crate A depends on `serde` with `features = ["derive"]`
and crate B depends on `serde` with `features = ["std"]`, then in the final
build `serde` is compiled once with `["derive", "std"]` enabled. Both A and B
link against that single artifact.

The edition-2021 resolver (`resolver = "2"`) changes this in one important way:
features requested only for dev-dependencies, build-dependencies, or for a
different `--target` are **not** unified into the normal build. That means the
same package can legitimately appear in the unit graph multiple times with
different feature sets (e.g. once for the host build script, once for the
target library).

### 1.3 How to discover the feature set per unit

There are two sources of truth, with very different stability guarantees:

#### Stable: `cargo metadata --format-version 1`

Produces JSON with:

- `packages[]` — every package with its declared `features` map.
- `resolve.nodes[]` — one entry per resolved package with a `features` array
  listing the features Cargo has decided to enable for that package after
  unification. Each node also has `deps[]` pointing to other resolved nodes.
- `resolve.root` / `workspace_members` — entry points.

This is sufficient for a **coarse** plan: one compilation per package with
the unified feature set. It is the right starting point for v0.

Limitations:

- It does **not** expose per-target (lib vs bin vs build-script) feature
  differences.
- With resolver v2 and build/dev deps, a single package may genuinely need to
  be compiled twice with different feature sets. `cargo metadata` collapses
  this into a single node.

#### Nightly: `cargo build --unit-graph -Z unstable-options`

Emits the exact internal unit graph Cargo would execute, with one entry per
unit. Each unit includes:

- `pkg_id`
- `target` (kind: lib / bin / custom-build / proc-macro / test / example)
- `features` (the exact list for this unit)
- `platform` (host vs target)
- `mode` (build / test / doc / check)
- `dependencies[]` referencing other units by index

This is the only authoritative source when you need to match Cargo's behaviour
exactly. It is gated behind nightly + an unstable flag, so any production tool
must either require nightly or accept the coarser `cargo metadata` view and
document the gap.

### 1.4 Rule for a parallel driver

- Key compilation units by `(pkg_id, target_kind, feature_set, host_or_target)`.
  Never assume "same crate name == same artifact".
- Treat each distinct key as its own DAG node. Different keys of the same
  package are independent nodes that can run in parallel.
- For v0 using only `cargo metadata`, collapse to one unit per package using
  the resolved feature set. Document this as a known approximation.

---

## Topic 2: Multiple build targets in a workspace

A Cargo workspace contains N packages. Each package declares one or more
**targets**. The target kinds have meaningfully different build rules.

### 2.1 Target kinds and their rules

| Kind          | Runs when?                              | Compiled for         | Notes |
|---------------|-----------------------------------------|----------------------|-------|
| `lib`         | Normal build                            | `--target` (or host) | The `rlib` / `dylib` most deps consume. |
| `bin`         | Normal build / when building that bin   | `--target` (or host) | Links the lib of its own package. |
| `proc-macro`  | As a dependency of another crate        | **Host**             | Compiled as a `cdylib` and `dlopen`ed by `rustc`. |
| `custom-build` (`build.rs`) | Before its parent package   | **Host**             | Compiled, then *executed*; its stdout drives env vars for the parent. |
| `test`        | `cargo test`                            | `--target` (or host) | Gets dev-dependencies merged in. |
| `bench`       | `cargo bench`                           | `--target`           | Similar to tests. |
| `example`     | `cargo build --examples`                | `--target`           | Optional. |

### 2.2 Why this breaks a naive "one node per package" DAG

Three compounding issues:

**a) Build scripts must compile and then RUN before the parent crate.**
`build.rs` is a Rust binary. We must compile it (host), execute it, capture
its stdout (`cargo:rustc-cfg=...`, `cargo:rustc-link-lib=...`, `cargo:rerun-if-*`,
`cargo:warning=...`, arbitrary `cargo:KEY=VALUE` metadata), and feed the
resulting directives into the parent crate's `rustc` invocation. This is a
two-phase operation (compile + execute) disguised as one node.

**b) Proc-macros must be compiled for the host, not the cross target.**
If the workspace is being cross-compiled to, say, `aarch64-unknown-linux-gnu`,
every `proc-macro = true` dependency must still be compiled for the local
host toolchain because `rustc` dlopens it during its own execution. This
means the **same package** can legitimately appear in the unit graph twice:
once as a host `proc-macro` and (if it also has a non-proc-macro consumer)
once as a target library.

**c) The same package can appear with different feature sets.**
As covered in Topic 1: with resolver v2, dev/build/target-specific features
create multiple distinct units for one package.

### 2.3 Implications for parallel compilation

- **Unit identity is not package identity.** Every scheduler node must be
  keyed by the `(pkg, target, features, host/target)` tuple.
- **Build scripts are two operations, not one.** The DAG must contain
  separate nodes for "compile build.rs" and "run build.rs". The parent
  crate's compile depends on the *run* node.
- **Host/target split.** In the cross-compilation case, proc-macros and
  build scripts form a sub-DAG compiled for the host. This sub-DAG can
  generally be parallelised against the target DAG — they share no output
  artifacts.
- **Env/stdout capture.** When we eventually actually run rustc, we must
  faithfully replay Cargo's build-script protocol: parse `cargo:*` lines
  from stdout and forward the correct `--cfg`, `-L`, `-l`, and `env:*`
  flags to dependent compilations. This is the hardest boring part of the
  whole project.
- **Dev dependencies** live on a separate edge class: they only apply to
  test / example / bench targets. Do not pull them into the normal lib
  build graph.

### 2.4 Scope for v0

v0 **explicitly ignores** all of this:

- Skip build scripts (delegate their compile+run to Cargo).
- Skip proc-macros as a special case (delegate).
- One node per package, using `cargo metadata`'s unified feature list.
- No cross-compilation handling.
- No dev/test/bench/example targets.

This gives a correct-ish **plan** (ordering of library compilations) which
is enough to demonstrate wavefront scheduling. The gap between this plan
and a real build is what Topics 1 and 2 describe; later versions will
close it.
