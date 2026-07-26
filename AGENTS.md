# AGENTS.md

## Language

- Respond to maintainers in Chinese unless they request another language.
- Public documentation, CLI text, protocol fields, and source text are English. Experiment inputs preserve their original language.
- Soft-wrap all Markdown prose. Never hard-wrap documentation.

## Python

- Require Python 3.14.
- Use `uv`, Ruff, BasedPyright strict, Pytest, and Complexipy.
- Keep every function at cognitive complexity 15 or lower.
- A docstring must add a responsibility, contract, reason, or boundary that code cannot express. Do not restate a name, signature, type, or implementation.
- Put direct operands before keyword-only policy and boolean parameters in public callables.
- Do not erase imports under `TYPE_CHECKING` with runtime `Any` or `object` fallbacks.

## Product invariants

- RunLab records execution facts. It does not judge experiment quality or choose a winner.
- Environment owns runtime capability. Task owns instructions, initial workspace, and corpus inputs.
- Accepted Runs preserve a terminal Run Record even when setup, execution, or collection fails.
- Credentials never enter images, public records, logs, or workspaces.
- CLI stdout is compact machine-readable JSON. Progress and diagnostics use stderr.
- New entities and commands require evidence from real experiments.
