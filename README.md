# RunLab

RunLab records reproducible Agent execution facts. Its fixed operation is:

```text
Environment + Task -> Run
```

An Environment owns one Agentic Runtime and its capabilities. A Task owns the
initial instruction, optional writable workspace, and declared read-only
references. RunLab does not accept an arbitrary command, judge experiment
quality, choose a winner, or replace Agent-authored orchestration.

## Directory protocol

An Environment contains the runtime:

```text
environment/
├── Dockerfile
└── environment.json   # optional
```

The image entrypoint reads `task.md` from stdin and runs the Agent in
`/workspace`. RunLab also mounts `/artifacts` as the only persistent task-output
directory. A runtime that writes native audit logs declares their container
target:

```json
{
  "name": "codex",
  "output_protocol": "codex-jsonl",
  "logs": {
    "target": "/root/.codex/sessions"
  },
  "credentials": [
    {
      "name": "codex",
      "kind": "file",
      "target": "/root/.codex/auth.json"
    }
  ]
}
```

RunLab mounts that target at `logs/runtime/`. For Codex, the Environment must not
use `--ephemeral`, because that disables the native session record.

A Task contains the task-owned inputs:

```text
task/
├── task.md
├── task.json          # optional read-only reference declarations
└── workspace/         # optional initial writable workspace
```

`task.md` must tell the Agent which deliverables to write under `/artifacts`.
An exit code of zero without an artifact is a `collection_failed` Run. The
workspace is copied to temporary storage for execution and deleted after
collection; it is never a final artifact.

An Experiment is the full Cartesian product of its packages:

```text
experiment/
├── experiment.json
├── environments/
│   └── <environment>/
└── tasks/
    └── <task>/
```

RunLab executes every Environment × Task pair with bounded concurrency.
Selection, repeats, ordering, adaptive scheduling, and judging stay in Agent
scripts composed from `runlab run start`.

## Credential protocol

An Environment declares opaque credential slots. RunLab does not know how to
log in to a provider or discover its host configuration:

```json
{
  "credentials": [
    {
      "name": "runtime",
      "kind": "file",
      "target": "/run/credentials/runtime"
    }
  ]
}
```

The caller materializes a private directory whose entries match the declared
names:

```text
credentials/
├── codex       # file
├── pi          # file
├── claude      # file
└── lark/       # directory
```

Pass the directory explicitly or through `RUNLAB_CREDENTIALS`:

```bash
runlab experiment run ./experiment --credentials /private/credentials
```

RunLab validates entry kinds and private permissions before accepting a Run,
then bind-mounts each entry read-only at its Environment-owned target. A runtime
entrypoint may copy a credential into ephemeral writable configuration or read a
token file into its own process environment.

RunLab itself never writes credential source paths, contents, or digests into a
public record, logs, artifacts, workspaces, or images. The logical name, kind,
and container target remain in `run.json` as execution facts. An Environment is
trusted with credentials it requests; read-only transport cannot prevent a
malicious runtime from printing or exfiltrating a credential.

## Retained Run protocol

Every accepted Run preserves a terminal record, including setup, execution, and
collection failures:

```text
run-.../
├── run.json
├── artifacts/
│   └── ...                    # task deliverables only
└── logs/
    ├── task.md
    ├── stdout.log
    ├── stderr.log
    ├── measurements.jsonl     # raw Docker observations
    └── runtime/               # native session/log files, when declared
```

`run.json` contains identities, policy, terminal outcome, container facts,
aggregated usage and resource measurements, plus content-addressed manifests for
Artifacts and Logs. Artifact quality evaluation consumes Task + Artifacts.
Process audit consumes Run Record + Logs.

Credentials and host source paths never enter public records. Declared inputs
and credentials are mounted read-only. CLI stdout is one compact JSON object;
progress and diagnostics go to stderr.

## CLI

```bash
uv run runlab environment check examples/smoke/environment
uv run runlab task check examples/smoke/task
uv run runlab run start \
  --environment examples/smoke/environment \
  --task examples/smoke/task \
  --output runs/smoke
uv run runlab experiment run <experiment-directory> --jobs 2
uv run runlab schema show run-record
```

## Development

RunLab requires Python 3.14 and Docker:

```bash
uv sync
uv run poe check
```
