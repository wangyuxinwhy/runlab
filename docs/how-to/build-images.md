# Build OCI Images for RunLab

RunLab consumes standard OCI Images; it does not build them. Use Docker Buildx or another OCI-compatible builder, then import the resulting OCI Image Layout into RunLab.

This guide describes a practical layering, configuration, Secret, and verification contract for Images intended to run coding Agents.

## Keep the build boundary ordinary

Keep the Dockerfile, package lists, checksums, and smoke program in normal build source. Do not introduce a RunLab-specific build DSL or make RunLab interpret a Dockerfile.

Prefer a single-platform, single-Manifest OCI archive for the current RunLab import surface:

```bash
docker buildx build \
  --platform linux/arm64 \
  --target agent \
  --provenance=false \
  --sbom=false \
  --output type=oci,dest=agent-linux-arm64.oci.tar \
  --file images/Dockerfile \
  images
```

`--provenance=false --sbom=false` keeps Buildx from adding attestation Manifests to an archive that RunLab currently expects to contain one Image Manifest. Do not also load the Image into a Docker image store unless another workflow needs that copy.

## Lock inputs that define identity

- Pin the parent Image by digest.
- Pin independently downloaded tools by version and verify their checksum.
- Make Agent CLI versions explicit build arguments or source constants.
- Treat an apt transaction as a resolved build input: the resulting OCI digest fixes its exact output, even when the Dockerfile alone cannot reproduce the same package set later.
- Rebuild and accept a new Manifest digest when any input changes. Do not reuse a name or claim identical content merely because the Dockerfile text is unchanged.

## Layer by reuse boundary

Use a small number of layers whose boundaries match reuse and change frequency:

```text
base → agent → repository → task
```

- `base` contains the operating system, language runtimes, package managers, version control, and common build and diagnostic tools.
- `agent` adds one Agent CLI. An all-in-one Agent Image can reuse one complete Agent layer and copy the other isolated installations from sibling build stages.
- `repository` contains one prepared source tree and its reusable dependencies.
- `task` contains only the task-specific files or mutation.

Do not put repositories, benchmark samples, model selection, Provider configuration, or task files in the common base. Avoid one layer per package and avoid a custom OCI assembler solely to make sibling Agent layers share descriptor identities.

Changing a large file causes the next OCI layer to contain the complete changed file, not a byte-level patch. Place large, stable inputs before frequently changing task content.

## Define the runtime user and writable paths

Use a numeric non-root identity in the Image Config so runtime generation does not depend on container-side name lookup. A common contract is:

```text
User:       1000:1000
HOME:       /home/agent
WorkingDir: /workspace
Artifacts:  /artifacts
Agents:     /opt/agents/<agent>
```

The user must be able to write its home, workspace, artifact directory, XDG cache/config/state directories, and the selected Agent's runtime state directory. Agent installations under `/opt/agents` should be root-owned and not writable by the runtime user.

Pre-create Agent state directories such as `/home/agent/.codex` or `/home/agent/.claude`. A Secret file mounted below a missing parent can otherwise cause the runtime-created parent to have the wrong owner. The directory contains no credential in the Image; it only provides the correct runtime ownership boundary.

## Make Image Config intentional

Set these Image Config values explicitly:

- numeric `User`;
- `WorkingDir`;
- ordinary environment such as `HOME`, locale, `PATH`, and XDG paths;
- `Entrypoint` and `Cmd` for a single-Agent Image;
- a neutral shell `Cmd` for an all-in-one Image where the caller must choose the Agent.

The Agent default should accept a task from stdin and run non-interactively. It may disable the Agent CLI's own approval or sandbox layer when RunLab is the external execution boundary. RunLab still owns OCI isolation, Secrets, timeout, and network control.

Network policy is not an Image Config field. Select it per Run with `runlab run start --network isolated|egress`.

## Keep credentials and caches out of layers

Never copy login state, API keys, subscription files, shell history, host configuration, or benchmark answers into an Image. Deliver credentials only at Run time with `--secret-env` or `--secret-file`.

Use BuildKit cache mounts for apt indexes, package downloads, npm cache, and compiler cache. Remove build logs and temporary files in the same `RUN` that creates them. If a build-time version check writes into an Agent state directory, mount that directory as tmpfs for the check, then create the required empty runtime directory outside that tmpfs-backed step.

Audit final layers for at least:

- populated user cache directories;
- compiler or package-manager caches;
- logs and build scratch;
- Agent credentials and authentication state;
- transient runtime paths.

## Import with selection metadata

Import the verified OCI archive and assign a local Catalog name:

```bash
runlab image import agent-linux-arm64.oci.tar \
  --name agent \
  --description 'Coding Agent on the shared RunLab base' \
  --label role=agent \
  --label agent=example
```

Description and labels help an Agent select an Image. They are caller-provided Catalog metadata, not verified capability claims, and do not change the OCI Manifest digest.

## Verify through RunLab

Do not treat a successful builder exit as runtime verification. Exercise the imported Image through the same RunLab boundary used by real tasks:

```text
image import
  → image get
  → run config generate
  → run start
  → filesystem get
```

The smoke program should fail on any mismatch and should verify:

- uid, gid, home, and working directory;
- exact tool and Agent versions;
- writable runtime and artifact paths;
- non-writable Agent installation paths;
- discoverable non-interactive CLI arguments;
- absence of preloaded credentials and populated caches.

Write a small structured result to `/artifacts`, then retrieve it from the Run's Final Environment:

```bash
runlab run config generate --image agent \
  | jq '.process.args = ["/bin/bash", "-s"]' \
  >smoke-config.json

runlab run start \
  --id 550e8400-e29b-41d4-a716-446655440000 \
  --image agent \
  --runtime-config smoke-config.json \
  --stdin smoke.sh

runlab filesystem get \
  --run 550e8400-e29b-41d4-a716-446655440000 \
  /artifacts/smoke.json \
  --output smoke-result.json
```

When testing a real credential path, also verify that the Run Record reports `retained: false` and that the Secret file is absent from the Final Environment. Preserve failed Runs as evidence instead of repeating until one succeeds and reporting only that result.

## Remove duplicate temporary storage

After `image get` resolves the Catalog name to the expected Manifest digest and the real Run succeeds, the imported archive is normally a duplicate of content already retained by the RunLab OCI Store. Delete that temporary archive unless it is intentionally kept as a distribution artifact. Manage Builder cache separately; it is rebuildable execution state, not a RunLab asset.
