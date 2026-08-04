# RunLab

RunLab records reproducible Agent execution facts through one fixed operation:

```text
Base + Overlay + Task -> Run
```

A **Base** owns one Agent runtime and its execution contract. An **Overlay** owns capability configuration — skills, installed tools, model selection, network access. A **Task** owns the instruction and the material it operates on. Composing Runs into experiments belongs to the caller, not to RunLab.

## Why the layers exist

Experiments require controlled variables, and a directory containing a `Dockerfile` is not a constant: base image tags move, package managers resolve to new versions, and a cold build cache produces a different image from the same file. When the environment drifts, every new variant forces re-running all controls, and the cost of an experiment stops being proportional to the new work.

RunLab separates a **declaration** from its **realization**. A declaration is source; a realization is what was actually built, addressed by digest and frozen in a lock file. Runs reference realizations, and a locked realization that can no longer be retrieved fails the Run instead of being silently rebuilt.

Separating Overlay from Base is what makes a difference between two Runs attributable. Capability configuration changes often; putting it in the Base would move the Base digest on every skill edit and make every historical Run appear incomparable.

## Try it

```bash
uv sync

uv run runlab base check   examples/bases/pi
uv run runlab base build   examples/bases/pi          # writes base.lock

uv run runlab run start \
  --base    examples/bases/pi \
  --overlay examples/overlays/skills-v1 \
  --task    examples/tasks/greet \
  --output  runs
```

`examples/` holds a working ablation: the same Base and Task with `skills-v1`, `skills-v2`, or no Overlay produce `HELLO`, `GREETINGS`, and `UNKNOWN`. The three Runs share a Base realization and a Task digest, so the only recorded difference is the Overlay.

The example Base runs [pi](https://www.npmjs.com/package/@earendil-works/pi-coding-agent) against DeepSeek and expects a `deepseek` credential; see the credential protocol in the reference layer.

## What a Run keeps

```text
run-…/
├── run.json      identities, realization chain, outcome, usage, manifests
├── inputs/       byte copies of the Base, Overlay, and Task declarations
├── artifacts/    task deliverables only
└── logs/         task.md, stdout, stderr, native runtime session
```

Declarations are copied in rather than referenced, because a digest proves that two things differ but reconstructs neither. Every accepted Run keeps a terminal record, including setup, execution, and collection failures.

## Documentation

The documentation site is organized as tutorial, how-to, reference, and explanation:

```bash
pnpm install && pnpm dev
```

Start with **Your first Run** to get an Agent executing, **Your first ablation** to see what the layering buys, and **Why the layers exist** for the reasoning behind the design.

Reference and explanation also ship inside the package, so guidance always matches the installed version:

```bash
uv run runlab docs list
uv run runlab docs get explanation/principles
uv run runlab schema show run-record
```

Tutorials and how-to guides are deliberately not served there — one-time setup costs an Agent context without changing what it can do.

## Development

RunLab requires Python 3.14 and Docker.

```bash
uv run poe check
```
