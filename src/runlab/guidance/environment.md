# Authoring a RunLab Environment

Use this guidance when packaging an Agent runtime for RunLab. An Environment owns runtime capability. A Task owns instructions, its initial writable workspace, and declared read-only references.

## Fixed Runtime Contract

An Environment is a directory:

```text
environment/
├── Dockerfile
└── environment.json
```

`Dockerfile` is required. `environment.json` is optional when every default is correct. The image entrypoint must:

1. Read the complete Task instruction as UTF-8 text from stdin.
2. Run the Agent with `/workspace` as its working directory.
3. Write final deliverables under `/artifacts`.
4. Exit nonzero when the Agent execution fails.

RunLab controls the container command. An Environment does not accept an arbitrary host command from a Task or Run.

## Filesystem Boundaries

RunLab mounts these writable directories:

| Container path         | Owner             | Retention                      |
| ---------------------- | ----------------- | ------------------------------ |
| `/workspace`           | Task and Agent    | Temporary                      |
| `/artifacts`           | Task deliverables | Retained as Artifacts          |
| Declared `logs.target` | Agent runtime     | Retained under `logs/runtime/` |

The Task's optional `workspace/` is copied into temporary storage before the Run. RunLab records its size at completion and then deletes it. Only files under `/artifacts` become formal deliverables.

An Agent exit code of zero without an artifact is a collection failure. The Task instruction must state which deliverables to write under `/artifacts`.

## Environment Definition

A complete definition can declare runtime logs, credentials, build inputs, runtime inputs, output protocol, and descriptive metadata:

```json
{
  "name": "claude-code",
  "output_protocol": "claude-stream-json",
  "logs": {
    "target": "/home/node/.claude/projects"
  },
  "credentials": [
    {
      "name": "claude",
      "kind": "file",
      "target": "/run/credentials/claude/setup-token"
    }
  ],
  "build_inputs": [],
  "inputs": [],
  "metadata": {
    "runtime": "claude-code"
  }
}
```

Unknown fields are rejected. Use `runlab schema show environment` for the version-matched JSON Schema.

## Native Runtime Logs

`logs.target` is the absolute container directory where the runtime naturally writes its richest native session or audit files. RunLab bind-mounts `logs/runtime/` at that target and retains the files without normalizing their format.

Examples:

```json
{ "logs": { "target": "/root/.codex/sessions" } }
```

```json
{ "logs": { "target": "/home/node/.claude/projects" } }
```

This declaration does not configure runtime logging. The image entrypoint must invoke the runtime in a mode that writes native logs. For Codex, do not use `--ephemeral`. A successful Agent execution with declared but empty native logs becomes a collection failure.

RunLab always retains process stdout, process stderr, the Task instruction, and raw Docker measurements separately. Native logs are not Artifacts.

## Output Protocol and Model Usage

`output_protocol` tells RunLab how to derive normalized model usage from the retained stdout log:

| Value                | Runtime stdout contract                      |
| -------------------- | -------------------------------------------- |
| `opaque`             | Preserve stdout without deriving model usage |
| `codex-jsonl`        | Parse Codex JSONL `turn.completed` events    |
| `claude-stream-json` | Parse the terminal Claude stream JSON result |

A structured output protocol requires `logs.target`, because RunLab preserves the native runtime session as the primary audit record. Protocol adapters only derive deterministic facts. They do not judge output quality.

## Credentials

An Environment declares opaque credential slots:

```json
{
  "credentials": [
    {
      "name": "claude",
      "kind": "file",
      "target": "/run/credentials/claude/setup-token"
    }
  ]
}
```

The declaration states the logical name, entry kind, and absolute container target. It must not contain a host path or credential value.

RunLab resolves credential entries from the first configured source:

1. `--credentials DIRECTORY`
2. `RUNLAB_CREDENTIALS`
3. `$XDG_CONFIG_HOME/runlab/credentials`
4. `~/.config/runlab/credentials` when `XDG_CONFIG_HOME` is unset

The credential root and every requested entry must not be accessible by group or others. RunLab validates all Environment credential requests before an Experiment accepts any Run, then mounts each entry read-only. It records only the logical name, kind, and container target.

RunLab does not create or modify the credential store. Create the root with mode `0700`; requested file entries normally use `0600`, and requested directory entries use `0700`.

An entrypoint may read a token file into the runtime process environment or copy a credential into ephemeral writable runtime configuration. Credentials must never enter images, workspaces, Artifacts, Logs, or public records.

The Environment is trusted with every credential it requests. A read-only mount does not prevent malicious runtime code from printing or exfiltrating a secret.

## Runtime Inputs

Environment `inputs` declare read-only host material needed at runtime:

```json
{
  "inputs": [
    {
      "name": "index",
      "source_env": "TRACE_INDEX_DB",
      "target": "/data/index.sqlite"
    }
  ]
}
```

The caller supplies the host source through `source_env`. RunLab records its content digest, kind, and container target, but not the host path. Use an input for runtime capability shared across Tasks. Task-specific references belong in `task.json`.

## Build Inputs

`build_inputs` provide reproducible external Docker build contexts:

```json
{
  "build_inputs": [
    {
      "name": "trace_index_source",
      "source_env": "TRACE_INDEX_SOURCE",
      "include": ["pyproject.toml", "src"]
    }
  ]
}
```

RunLab snapshots selected entries when `include` is present, records the digest, and passes the snapshot as a named Docker build context. The Dockerfile consumes it with a named-context `COPY` instruction. Never use build inputs for credentials.

## Measurements

RunLab owns engine-observable measurements. The Environment does not declare commands, scripts, or a measurement DSL.

RunLab samples and retains raw Docker observations in `logs/measurements.jsonl`, then derives:

- wall time;
- peak CPU, memory, and process count;
- maximum observed cumulative network and block I/O;
- workspace, Artifact, and Log byte counts;
- normalized model usage when `output_protocol` is known.

Raw Logs remain the audit source. Aggregated measurements in `run.json` are compact execution facts. Missing facts remain explicit rather than guessed.

## Validation Checklist

1. Build the image without embedding credentials or mutable host state.
2. Ensure the entrypoint reads stdin and runs in `/workspace`.
3. Ensure the Task can write only formal deliverables to `/artifacts`.
4. Declare and verify the runtime's native log directory.
5. Select a structured output protocol only when stdout matches it.
6. Declare the minimum credential and input slots.
7. Run `runlab environment check ENVIRONMENT`.
8. Execute one realistic Task and inspect its Artifact, native session, stdout, stderr, raw measurements, and `run.json`.
