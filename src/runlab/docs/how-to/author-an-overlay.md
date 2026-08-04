# Author an Overlay

An Overlay changes what the Agent can do, on top of a given Base. Write one whenever the thing you want to vary is a capability rather than a runtime or a task.

## Pick the delivery form

The form follows from what the capability physically is. All four are the same kind of experimental variable and are recorded in the same field.

| The capability is | Use | Realization |
| --- | --- | --- |
| Software to install or remove | `layer` | Image digest, tied to the Base |
| Files the Agent reads | `mounts` | Content digest, independent of the Base |
| A setting the entrypoint reads | `env` | Part of the environment key |
| Container-level access | `network` | Part of the environment key |

They combine. One Overlay may install a tool, mount a skill, and set a model.

## Mount files

```text
overlays/skills-v2/
├── overlay.json
└── files/skills/greeting/SKILL.md
```

```json
{
  "name": "skills-v2",
  "mounts": [{ "source": "files/skills", "target": "/opt/runlab/skills" }]
}
```

The target must be a path the Base already looks at. Mount-only Overlays are never built, so they have no lock and iterate instantly — which is what you want when tuning a skill.

## Add or remove software

```dockerfile
ARG BASE_IMAGE
FROM ${BASE_IMAGE}

RUN apt-get update \
    && apt-get install --yes --no-install-recommends python3 \
    && rm -rf /var/lib/apt/lists/*
```

```json
{ "name": "with-python", "layer": "Dockerfile" }
```

The `ARG BASE_IMAGE` / `FROM ${BASE_IMAGE}` opening is required; RunLab injects the Base realization. Removal is a first-class use — deleting an interpreter to study its absence is a capability experiment, not a broken image.

Build it against the Base it will be used with:

```bash
uv run runlab overlay build overlays/with-python --base bases/pi
```

## Set configuration

```json
{ "name": "pro-model", "env": { "PI_MODEL": "deepseek-v4-pro" } }
```

This only works if the Base entrypoint reads that variable. Check the Base before assuming a name.

## Toggle access

```json
{ "name": "offline", "network": "none" }
```

`network` belongs here rather than in policy because connectivity determines what the Agent can do. Policy carries resource bounds — `--timeout-seconds`, `--memory`, `--cpus` — which limit how much the Agent can do without changing what it is able to do.

## Stack them

```bash
uv run runlab run start --base bases/pi \
  --overlay overlays/skills-v2 \
  --overlay overlays/with-python \
  --task tasks/analyze --output runs
```

Order is significant and participates in identity: `a` then `b` is a different environment from `b` then `a`.

An Overlay that declares nothing normalizes away, so keeping an empty `baseline` Overlay for the control arm of an ablation is safe — it produces the same environment as passing no Overlay at all.

## Watch for a broken contract

A layer Overlay can break the Base. Removing an interpreter the Agent itself needs surfaces as `agent_failed`, which reads like task difficulty rather than a broken environment.

After building a subtractive Overlay, run it against a trivial Task first and confirm the Run succeeds before drawing conclusions from a real one.
