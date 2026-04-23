# parallel-rustc

Wavefront parallel compilation planner for Cargo workspaces.

## Status

v0.0.0 — prints the plan, does not invoke `rustc` yet.
See [`spec/0.0.0.md`](spec/0.0.0.md) for the versioned spec and
[`research/topics.md`](research/topics.md) for background.

## Usage

```
cargo run -- plan [--manifest-path <PATH>]
```

Example output:

```
parallel-rustc plan
workspace: /path/to/ws
packages: 34   phases: 9

phase 0 (16 packages, parallel):
  - anstyle v1.0.14
  - ...
phase 1 (4 packages, parallel):
  - ...
...
```

Each phase lists packages that can compile in parallel because all of
their dependencies are already built in earlier phases (layered Kahn's
algorithm over the `cargo metadata` resolve graph).

## Limitations (v0)

- No actual compilation.
- No build scripts / proc-macros / dev-deps handled specially.
- One node per package, not per `(target, features)` unit.

See `research/topics.md` for why these matter and what the fix looks like.
