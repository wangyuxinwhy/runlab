---
title: CLI Reference
description: RunLab command families, discovery path, and side-effect boundaries.
---

# CLI Reference

Use the running binary as the exact command source:

```bash
runlab --help
runlab COMMAND --help
runlab COMMAND SUBCOMMAND --help
```

The public command families are:

| Command | Responsibility |
| --- | --- |
| `docs` | Read version-matched Markdown without opening State. |
| `vm` | Manage the fixed macOS Linux VM. |
| `image` | Import, inspect, list, and export OCI Images. |
| `filesystem` | Read paths or changes from an Image or Final Environment. |
| `exec` | Execute synchronously without creating a persistent Run. |
| `run` | Generate configuration and create, inspect, cancel, reconcile, or delete persistent Runs. |
| `observation` | Register immutable Observation Types and append, correct, or retract terminal-Run Observations. |
| `schema` | Discover public SQL Relations and their columns. |
| `query` | Execute one bounded read-only SQL statement. |
| `storage` | Inspect State usage and explicitly check or apply safe pruning. |

Read commands do not execute, synchronize, repair, reconcile, or mutate State. Commands that permanently delete Run assets or reclaim storage use explicit check/apply workflows.

On macOS, `vm config get` returns the normalized complete share declaration. `vm config check --document FILE` validates it and reports derived Guest paths, warnings, required changes, and whether the stopped VM can accept it. `vm config apply --document FILE` replaces the declaration only while the VM is stopped. Each input share contains only `name` and a macOS directory `host_path`; the output derives `guest_path`, `type: "virtiofs"`, and `read_only: true`.
