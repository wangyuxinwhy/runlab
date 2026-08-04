# Architecture

The package layering is an hourglass. `core` is the protocol waist at the bottom, `engine` is the composition waist above it, and concept domains spread flat between them with no edges to each other. `tach.toml` at the repository root is the executable form of this contract; this document states the reasoning a path alone cannot carry.

## Layers

```text
cli
  └── engine
        ├── declarations   loading and validating Base, Overlay, Task sources
        ├── realization    lock semantics and the realization chain
        ├── container      Docker particulars
        ├── usage          output protocol adapters
        └── record         Run directory layout, snapshots, manifests
              └── core     protocol vocabulary, identity, errors
```

`core` owns the vocabulary every other package exchanges: declaration models, identity types, lock models, the Run record, and content digest. It depends on nothing. Its upward surface is a declared interface rather than a consequence of file placement, so admitting a name into the shared protocol stays a reviewed decision.

Domains own one concept each and never import one another. `realization` owns what a lock means, not how an image is built. `container` owns how Docker is invoked, not what a built image signifies. Keeping these apart is what allows the container engine to change without touching lock semantics, and it is the reason `realization` has no Docker vocabulary in it at all.

`engine` sequences domains through their public interfaces and defines no entity that crosses a boundary. Composition is not carrying: dispatching to a domain keeps that domain's knowledge at home, while re-implementing its formats or heuristics moves knowledge out of its owning package.

`cli` is the agent-facing surface. It maps commands to engine and domain entry points, emits compact JSON on stdout, and sends progress to stderr.

## Admission

A module belongs to the package that owns its concept. Formal properties such as being free of I/O or being widely imported do not qualify it: broad consumption is reachability, not ownership.

Two failures have specific homes in this design. Knowledge from one Agent runtime — how a particular CLI formats its output — belongs in `usage`, never in `core`, even though the resulting `ModelUsage` is protocol vocabulary. Knowledge about how Docker reports a container state belongs in `container`, never in `engine`, even though `engine` decides what to do with the outcome.

A domain-free helper carrying no project concept lives as a top-level utility module outside every package.

## Verification

The single verification entry point is `uv run poe check`, which runs `ruff format`, `ruff check`, `basedpyright`, `pytest`, `tach check`, and `complexipy` in that fixed order and fails fast.

`tach check` is not a style gate. A cross-domain import is a design error that compiles and passes tests, so it needs a check that fails on structure rather than on behavior.

## Process Boundary

RunLab invokes Docker, Git, and registry operations through their command-line interfaces rather than through SDKs. A CLI surface is more stable across versions than a client library, its behavior matches what a person reproduces by hand, and it keeps the host tool chain responsible for authentication. This choice is why the implementation language has little leverage over the design: nearly all external capability arrives through a subprocess boundary.
