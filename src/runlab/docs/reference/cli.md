# CLI Reference

Every command prints one compact JSON object on stdout and sends progress and diagnostics to stderr. Exit status is `0` on success, `1` on a RunLab error, and `2` on invalid usage.

## Declarations

```bash
runlab base check <directory>
runlab overlay check <directory>
runlab task check <directory>
```

Validate one declaration directory and print its identity. `check` never builds and never writes, so it is safe to run against an unlocked declaration.

## Realization

```bash
runlab base build <directory>
runlab overlay build <directory> --base <base-directory>
```

Fix a declaration into a realization and write its lock. An Overlay realization depends on the Base it is applied to, so `overlay build` requires a Base whose lock already exists and adds one entry per Base realization.

A mount-only Overlay has no image to build; its realization is the content digest of its mounted trees, and it needs no lock.

Both commands are idempotent: a declaration whose lock already resolves to a retrievable realization is reported as reused rather than rebuilt.

## Execution

```bash
runlab run start \
  --base <directory> \
  [--overlay <directory>]... \
  --task <directory> \
  [--output <directory>] \
  [--timeout-seconds <n>] [--memory <size>] [--cpus <n>] \
  [--credentials <directory>] \
  [--rebuild]
```

Execute one Run and print `run_id`, `outcome`, and the record path. `--overlay` may be repeated and is order-sensitive.

Resolution fails when a lock exists but its realization cannot be retrieved. `--rebuild` accepts that drift and rebuilds, which produces a realization that is not comparable with earlier Runs.

Exit status is `1` when the outcome is `setup_failed` or `collection_failed`, so a caller scripting a matrix can distinguish an infrastructure problem from an Agent that simply failed the task.

## Documentation and Schema

```bash
runlab docs list
runlab docs get <topic>
runlab schema list
runlab schema show <name>
```

`docs` serves the reference layer bundled with the installed CLI, so the guidance always matches the binary being called. `schema` prints JSON Schema generated from the public models.
