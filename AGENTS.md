# AGENTS.md

## Worktree Purpose

This worktree is the clean rewrite of RunLab around the new Run Protocol.

The sibling worktree at `/Users/bytedance/workspace/temp/runlab` contains the legacy `Base + Overlay + Task -> Run` implementation. Treat it only as implementation evidence and a source of reusable low-level mechanics. Do not edit it from this worktree, preserve its user-owned changes, and do not carry its public model forward for compatibility.

Do not create a parallel `runlab2`, `next`, compatibility package, or second public protocol. This worktree must converge on one `runlab` product surface.

## Sources of Truth

Use this order of authority:

1. Agent Wiki owns settled product, protocol, system-design, and architecture decisions.
2. Files in this worktree own the current implementation, executable tests, local implementation plan, temporary engineering notes, and work in progress.
3. The sibling legacy worktree owns only facts about the old implementation.

Do not copy the full protocol or temporary implementation status into this file. A settled design change belongs in Agent Wiki. An implementation plan, checkpoint, open engineering question, or temporary decision belongs in an ordinary file in this worktree until it becomes stable enough to promote into the design documentation.

When Agent Wiki and inherited source or documentation disagree, Agent Wiki defines the target design. Current executable behavior must still be reported honestly as implementation state.

## Required Design Reading

Read the relevant Agent Wiki pages before design or implementation work:

- Documentation index: `http://localhost:8787/app/pages/runlab-index--nw`
- Run Protocol: `http://localhost:8787/app/pages/runlab-core-model--gt`
- OCI Images and Local Catalog: `http://localhost:8787/app/pages/runlab-oci-image-catalog--jl`
- System design: `http://localhost:8787/app/pages/runlab-system-design--ly`
- Execution Backend Contract: `http://localhost:8787/app/pages/runlab-execution-backend--sv`

For Agent access, fetch page content from `http://localhost:8787/pages/<page-id>/content`. Read open comments when changing a documented area. Give maintainers the Human UI `/app/pages/...` address rather than an API address.

When prior decisions, corrections, experiments, failures, or provenance matter, use `trace-index` instead of relying on memory. If Agent Wiki is unavailable, do not reconstruct settled design from the inherited legacy documentation alone.

Any documentation inherited under `src/runlab/docs` describes the legacy product and is not target-design authority. Replace or remove it only as corresponding behavior becomes real in the new implementation.

## Local Working State

Keep implementation plans and temporary design notes in this worktree, not Agent Wiki. Prefer a small number of plainly named root-level Markdown files when they become necessary; do not create a documentation hierarchy for transient work.

Before acting, inspect the current worktree, its local plans, and the nearest applicable instructions. Update local plans as implementation facts change. Remove or promote temporary notes when they are no longer useful so they do not become an accidental second specification.

Do not let the inherited language, package layout, models, tests, or architecture select the new implementation by default. Implementation language and physical structure require an explicit decision. Reuse code only when it satisfies the target design without importing legacy vocabulary or compatibility constraints.

## Working Rules

- Respond to maintainers in Chinese unless they request another language. Keep public protocol fields, CLI text, and code-internal names in English.
- Design Agent-facing CLI behavior only after reading `/Users/bytedance/.agents/guidance/agent-friendly-cli.md`.
- Read `/Users/bytedance/.agents/guidance/experiment-driven-development.md` before changing an Agent tool or abstraction based on experiments.
- Prefer ordinary Linux concepts and atomic, discoverable operations over RunLab-specific DSLs.
- Add public vocabulary or extension points only after a real workflow demonstrates the need.
- Separate execution facts from judgments in implementation, tests, documentation, and reports.
- Preserve failures, contaminated comparisons, and unsupported assumptions as first-class findings.
- Preserve user-owned and unrelated changes. Use non-destructive Git operations and do not add `Co-Authored-By` trailers.
- Do not commit, merge, publish, or release unless explicitly requested.
- Do not spawn an independent reviewer or subagent unless the user explicitly asks for one.

## Rust Implementation

- Maintain one Rust 2024 workspace with exactly three packages: the `run_protocol` library, the `run_engine` library, and the `runlab` binary. Keep one public `runlab` product surface. Do not add a Python compatibility package, async runtime, SDK wrapper service, ORM, or parallel protocol package.
- Keep `rust-toolchain.toml` fixed for reproducible development and keep `package.rust-version` as the tested MSRV. `unsafe` is forbidden.
- Prefer blocking standard-library process, filesystem, and thread primitives. OCI content identity is computed from exact bytes; typed JSON views never replace retained content-addressed bytes.
- `NativeEngine` and `DockerEngine` implement `RunEngine` directly. Keep their runtime mechanics behind narrow subprocess boundaries, and add no additional generic backend trait unless both real implementations demonstrate a smaller stable private boundary.
- Run `cargo fmt --check`, all-target tests, Clippy with warnings denied, the declared MSRV check, packaging, separate-process CLI contract tests, and the opt-in real Docker test before claiming completion.
- Comments must explain a non-obvious invariant, safety boundary, protocol decision, or justified lint exception. Delete comments that merely restate the adjacent name, type, or control flow. Clap doc comments are user-facing help text and must earn their place as interface documentation.
- Preserve the package dependency direction `runlab -> run_engine -> run_protocol`, with `runlab` also allowed to depend directly on `run_protocol`. `run_protocol` owns no execution, product, or persistence concepts. `run_engine` owns no `run_id`, Run Record, Catalog, database, or recovery interface. Keep exact-byte content access narrow; do not let CLI own lifecycle semantics, Engine publish Run Records, or Storage interpret process outcomes.

## Verification

Use verification appropriate to the implementation language and current vertical slice. Before reporting implementation complete, test the installed CLI as a separate process, verify stdout, stderr, exit status, invalid input, and interruption behavior, and run at least one deterministic end-to-end Run through the real Docker backend.

Report implemented behavior, verification evidence, failures, and remaining risk separately. Do not turn current behavior into a test contract merely because the inherited implementation does it.
