# AGENTS.md

## Language

- Respond to maintainers in Chinese unless they request another language.
- Public documentation, CLI text, protocol fields, and code-internal text are English. Experiment inputs preserve their original language.
- Write prose one paragraph per line in every language and let editors soft-wrap. Hard wraps belong only inside code blocks; in CJK a wrapped paragraph also renders a spurious space at the break.
- CJK prose uses fullwidth punctuation. A halfwidth mark between two CJK characters is a typo, not a style choice.

## Coding Conventions

- Never add `from __future__ import annotations`; the codebase targets Python 3.14+.
- Keep the cognitive complexity of every function at 15 or below.
- Declare pydantic defaults with bare assignment; a `Field()` call must carry more than a default, such as a constraint, a pattern, or an alias.
- A docstring must add a responsibility, contract, reason, or boundary that the code cannot express. Do not restate a name, signature, type, or implementation.
- Comments explain why instead of restating what, speak at the abstraction level of the code they annotate, and state their constraint in place. Never anchor meaning in a commit, ticket, or document reference, and never bind a neutral abstraction to one concrete runtime.
- Place direct operands before keyword-only policy and boolean parameters in public callables, and place equivalent parameters the same way across sibling callables.
- Do not erase `TYPE_CHECKING` imports with runtime `Any` or `object` fallbacks.
- Place every module by concept ownership, never by formal properties such as being I/O-free. A domain owns one concept and imports no other domain; `engine` sequences domains without re-implementing them; only `core` carries vocabulary that crosses boundaries.

## Product Invariants

- RunLab records execution facts. It does not judge experiment quality, score artifacts, or select a winner.
- The operation is fixed. RunLab never accepts an arbitrary command from a Task or a caller.
- Base owns the Agent runtime and its execution contract, Overlay owns capability configuration, Task owns the instruction and its material.
- An accepted Run preserves a terminal record even when setup, execution, or collection fails.
- Every derived value in a record must be reconstructible from the evidence retained beside it.
- A missing realization fails the Run. Never rebuild silently to recover from it.
- Credentials never enter images, public records, logs, workspaces, or artifacts.
- CLI stdout is one compact machine-readable JSON object. Progress and diagnostics use stderr.
- New entities, fields, and commands require evidence from real experiments.

## Required Reading

- [Design Principles](src/runlab/docs/reference/principles.md) states the guarantees, the invariant tiers, and the admission test for anything new.
- [Model](src/runlab/docs/reference/model.md) is the source of truth for declarations, identity, locks, and the Run record.
- [Architecture](src/runlab/docs/reference/architecture.md) states package layering and the verification chain.

## Documentation

- All documentation lives in one tree under [src/runlab/docs](src/runlab/docs), organized as tutorial, how-to, reference, and explanation. The tree serves both the VitePress site and `runlab docs`, so a relative link between two documents resolves identically in either. `docs/` holds only `.vitepress`, which keeps site build output out of the Python package.
- `runlab docs` serves reference and explanation only. Reference states mechanisms an Agent must not guess at; explanation states the constraints behind them. Tutorials and how-to guides are deliberately withheld: one-time setup and human workflows cost an Agent context without changing what it can do.
- Reference and explanation are the source of truth for mechanisms and their rationale, derived from the implementation. A change touching `src/` must leave them consistent in the same change, and their relative links stay within those two layers so the shipped pair remains self-contained.
- A document carries only the main line of its stated topic. Side-path content such as one-time setup or another document's mechanism is linked, not inlined, and a tutorial leads with the action rather than with installation.
- Prose carries no removable content: a repeated statement, a filler transition, a content-free summary, and over-explanation of the obvious are deleted. Emphasis must carry distinguishing information, and numbering is used only when the number is the content.
- Verify the site with `pnpm build`; it fails on dead links, which is what keeps cross-layer references honest.

## Verification

- `uv run poe check` is the single verification entry point: `ruff format`, `ruff check`, `basedpyright`, `pytest`, `tach check`, `complexipy`, in that order, fail-fast.
- Test the installed CLI as a separate process, not only its internal functions. Verify stdout shape, stderr, exit status, and invalid-input behavior.
