# Model

RunLab has three declaration kinds, two identity layers, and one terminal record. The layering exists so that a variable under study lands in exactly one place, which is what makes a difference between two Runs attributable. [Why the layers exist](../explanation/why-layers.md) states why; this document states what.

## Declaration Layers

The layers are separated by the role a change plays in an experiment, never by how the change happens to be delivered.

| Layer | Owns | Changes |
| --- | --- | --- |
| Base | Which Agent runs and how it is invoked | Rarely |
| Overlay | What the Agent is configured to be able to do | Often |
| Task | What to do and with which material | Often |

Placing capability configuration in Base would force a full rebuild for every skill edit and would move the Base digest, making every historical Run appear incomparable. Placing it in Task would disguise a capability experiment as a change of task.

### Base

A Base owns one Agent runtime and its execution contract.

```text
bases/<name>/
├── Dockerfile
├── base.json
└── base.lock        # generated
```

The image entrypoint reads the complete Task instruction as UTF-8 from stdin, runs the Agent with `/workspace` as the working directory, writes deliverables under `/artifacts`, and exits nonzero when execution fails. RunLab controls the container command; a Base never accepts an arbitrary command from a Task or a caller.

```json
{
  "name": "pi",
  "output_protocol": "pi-session-jsonl",
  "logs": { "target": "/root/.pi/sessions" },
  "credentials": [
    { "name": "deepseek", "kind": "file", "target": "/run/credentials/deepseek" }
  ],
  "build_inputs": [],
  "inputs": [],
  "metadata": {}
}
```

`output_protocol` selects how normalized model usage is derived and must be one of `opaque`, `codex-jsonl`, `claude-stream-json`, or `pi-session-jsonl`. Any protocol other than `opaque` requires `logs.target`, because the native session is the primary audit record and the derived usage must remain reconstructible from it.

### Overlay

An Overlay owns everything that determines what the Agent can do, on top of a given Base. It has four delivery forms, and the form is an implementation detail of the Overlay rather than a classification of the capability.

```text
overlays/<name>/
├── overlay.json
├── Dockerfile       # optional, layer form
├── files/           # optional, mount form
└── overlay.lock     # generated, layer form only
```

```json
{
  "name": "trace-index-1.2+skills-v2",
  "layer": "Dockerfile",
  "mounts": [{ "source": "files/skills", "target": "/root/.pi/skills" }],
  "env": { "PI_MODEL": "deepseek/deepseek-v4-flash" },
  "network": "default"
}
```

The layer form installs or removes software and produces a new image. Its `Dockerfile` must begin with `ARG BASE_IMAGE` followed by `FROM ${BASE_IMAGE}`; RunLab injects the Base realization. Removal is a first-class use — deleting an interpreter to study its absence is a capability experiment, not a broken environment.

The mount form supplies read-only files such as skills and instructions. It produces no image, so its realization is the content digest of the mounted trees and is independent of the Base.

The env form supplies environment variables that the Base entrypoint consumes. Model selection and tool allowlists arrive this way, which keeps the execution contract owned by the Base.

The capability form toggles container-level access. `network` lives here rather than in policy because connectivity determines what the Agent can do, and a variable under study must sit with the other capability variables.

An empty Overlay and an absent Overlay are the same thing. Both normalize to an empty overlay list, so they cannot produce two declarations with one realization.

Overlays are ordered and may be stacked. Order participates in identity: applying `a` then `b` is a different realization from `b` then `a`.

A layer Overlay can break the Base execution contract — removing an interpreter the Agent itself needs surfaces as an Agent failure that looks like task difficulty. `overlay build` therefore runs a trivial smoke Task against the built image and fails the build when the contract no longer holds.

### Task

A Task owns the instruction and the material it operates on.

```text
tasks/<name>/
├── task.md
├── task.json        # optional
└── workspace/       # optional
```

`task.md` must state which deliverables to write under `/artifacts`. An exit code of zero without an artifact is a collection failure. The workspace is copied to temporary storage for execution and deleted after collection; only `/artifacts` produces deliverables.

A Task has no lock, because it is never built. Its content digest is its complete identity.

## Identity

### Declaration digest

The recursive content digest of a declaration directory. It answers "is this the same source", and it is stable across machines and backends.

### Realization

What actually ran. For a Base or a layer Overlay this is a platform-specific image digest; for a mount Overlay it is the content digest of the mounted trees.

A declaration digest is not a realization. The same `Dockerfile` builds different images on different days, so comparability is judged on the realization chain alone:

```text
base realization -> [overlay realizations, ordered] -> env -> capabilities
```

The record carries the full chain and an `environment_digest` folded from it, so "did these Runs share an environment" is one value comparison rather than a field-by-field walk.

Multi-platform manifests defeat byte-level recovery, because one manifest digest resolves to different images per architecture. Realizations record the platform-specific digest, and the platform is part of the recovery precondition.

## Lock

A lock file freezes a declaration into a realization. It is generated by `build`, committed alongside the declaration, and is the only mechanism that makes an environment a constant across time and machines. A lock is excluded from the digest of the declaration it accompanies, since a digest that changed as a consequence of being recorded could never be recorded.

The map key states what the realization depended on. A Base resolves per platform, so `base.lock` keys by platform:

```json
{
  "schema_version": 1,
  "declaration": "sha256:…",
  "realizations": { "linux/arm64": "sha256:image…" }
}
```

An Overlay layer is built on top of a specific Base realization, which is already platform-specific, so `overlay.lock` keys by Base realization:

```json
{
  "schema_version": 1,
  "declaration": "sha256:…",
  "realizations": { "sha256:base-image…": "sha256:overlay-image…" }
}
```

Entries accumulate: an entry that disappears takes the reproducible baseline of every Run that used it. Editing a declaration starts a fresh entry set, because the recorded entries describe source that no longer exists at that path. `--rebuild` is the one path that replaces an existing entry, which is why it must be requested explicitly — the rebuilt realization is generally not identical to the one it replaces.

A mount-only Overlay is never built, so it has no lock. Its realization is the content digest of the trees it supplies, which is independent of any Base.

Building an image on top of a locked realization needs a locally resolvable name for it, because a builder reads a bare digest in `FROM` as a registry reference. RunLab tags each realization before use; the tag is derived from the realization and carries no identity of its own.

Resolution has three outcomes, and the third is the reason the mechanism exists.

| State | Behavior |
| --- | --- |
| No lock | Build, write the lock |
| Lock present, realization retrievable | Use it |
| Lock present, realization missing | Fail; `--rebuild` is required to accept the drift |

Rebuilding silently is how a control group changes without anyone noticing. Failing turns drift into an act a person has to authorize.

## Policy

Policy carries resource bounds and termination conditions: `timeout_seconds`, `memory`, and `cpus`. It never carries capability. Insufficient memory exhausts a resource; a missing interpreter removes an ability. Only the latter belongs to an Overlay.

## Run Record

Every accepted Run retains a terminal record. Acceptance happens after inputs resolve, so an unresolvable declaration or an unretrievable realization fails before a Run exists and produces no record.

```text
run-…/
├── run.json
├── inputs/
│   ├── base/                  # declaration copy
│   ├── overlays/<name>/       # declaration copies, ordered
│   └── task/                  # task.md, task.json, workspace/
├── artifacts/
└── logs/
    ├── task.md
    ├── stdout.log
    ├── stderr.log
    ├── measurements.jsonl
    └── runtime/               # native session files, when declared
```

`inputs/` holds byte copies of the declarations, not just their digests. A digest proves that two things differ; it cannot reconstruct either one. Without the copies a Run is an asset only for as long as the source directories happen to survive unchanged.

`run.json` carries identities, the full realization chain, policy, terminal outcome, container facts, aggregated measurements, and content-addressed manifests for artifacts and logs.

Outcomes are `succeeded`, `agent_failed`, `timed_out`, `oom_killed`, `setup_failed`, and `collection_failed`. A Run whose Base declares a usage-aware `output_protocol` but yields no parsable usage is `collection_failed`: usage is a core asset, and an incomplete asset must not be reported as success.

## Credentials

A Base or an Overlay declares opaque credential slots by logical name, entry kind, and absolute container target. A declaration never contains a host path or a credential value.

The caller materializes a private directory whose entries match the declared names. RunLab resolves it from `--credentials`, then `RUNLAB_CREDENTIALS`, then `$XDG_CONFIG_HOME/runlab/credentials`, then `~/.config/runlab/credentials`. The root and every requested entry must be inaccessible to group and others. RunLab never creates or modifies this store, validates every slot before accepting a Run, and mounts each entry read-only.

Only the logical name, kind, and container target enter the record. The Base is trusted with whatever it requests: a read-only mount does not stop a malicious runtime from printing a secret.

These are distinct from the credentials RunLab itself needs to reach a registry or a remote store. Those never pass through this mechanism and are delegated to the host tool chain.

## Inputs

Base `inputs` declare read-only host material needed at runtime, supplied through `source_env` and recorded by content digest, kind, and container target — never by host path. `build_inputs` supply named Docker build contexts by the same mechanism.

Task `inputs` declare task-specific read-only references. Large material declared this way is recorded but not archived, so the record marks it as referenced rather than recovered.
