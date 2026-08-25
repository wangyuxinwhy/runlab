---
name: repo-repair-workflow
description: Diagnose and repair a repository task with executable tests when the requested outcome requires code changes rather than an explanation.
---

# Repository repair workflow

Read the task and the repository's package or build metadata before editing.

Run the existing tests to reproduce the failure. Inspect the smallest relevant production code and tests, then identify the behavior or invariant that the implementation violates.

Change production code rather than weakening, deleting, or bypassing tests. Keep the patch scoped to the requested behavior and preserve unrelated public behavior.

Run targeted tests after the change, then run the complete declared test command. If a test still fails, use its exact output to continue diagnosis instead of guessing.

Finish only after the requested files are changed and the full test command succeeds. Do not merely describe a proposed patch.
