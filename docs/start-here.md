# Start Here

RunLab executes a standard OCI Image with an OCI Runtime Configuration, stdin, Secrets, and explicit Run controls. Use `exec` for a synchronous disposable inspection. Use `run start` when the execution must become an immutable Run asset with a Final Environment.

```text
OCI Image + Runtime Configuration + stdin + Secrets + controls
  → exec
  → RunOutput

OCI Image + Runtime Configuration + stdin + Secrets + controls
  → run start
  → immutable Run Record + Final Environment
  → run get/query + filesystem get
```

RunLab consumes OCI Images but does not build them. See `runlab docs get how-to/build-images` when preparing one.

## Check the local execution plane

On macOS, confirm that the managed Linux VM is ready:

```bash
runlab vm status
```

If it is absent or stopped, discover the exact recovery commands with `runlab vm --help`. Linux runs `NativeEngine` directly and has no `vm` command.

## Select an Image

List the local Catalog, then inspect the chosen name and immutable Manifest digest:

```bash
runlab image list
runlab image get base
```

Catalog descriptions and labels help selection but are caller-provided metadata, not verified capabilities. The Manifest digest is the content identity.

## Generate and modify ordinary OCI JSON

Generate the complete Runtime Configuration from the Image Config and RunLab's fixed Linux scaffold:

```bash
runlab run config generate --image base >base.json
jq '.process.args = ["python", "-c", "print(\"ready\")"]' base.json >config.json
```

RunLab does not invent a command DSL. Edit the standard JSON with `jq` or another JSON tool. Network is a Run control selected with `--network isolated|egress`; it is not written into `config.json`.

On macOS, an explicit OCI bind mount source denotes a local Host file or directory. It must be absolute and contain the `ro` option. RunLab transfers it into the execution unit's private VM mount namespace without changing the Runtime Configuration bytes. Writable Host bind mounts are rejected because their writes cannot be faithfully reflected to macOS.

## Observe before preserving

Use `exec` to inspect an Image or command when no persistent identity or Final Environment is needed:

```bash
runlab exec --image base --runtime-config config.json
```

The complete bounded result is written to stdout. stderr is an NDJSON Live Event stream containing Run stages and Program stdout/stderr. `exec` has no ID and cannot be queried or resumed. Its side effects are real, so do not present a later persistent Run as the first attempt when the same evaluation task was already used with `exec`.

Add input and controls only when needed:

```bash
runlab exec \
  --image pi \
  --stdin prompt.txt \
  --network egress \
  --secret-env API_KEY \
  --secret-file ./auth.json=/home/agent/.agent/auth.json
```

Secrets are read from the caller and are not retained through their Secret fields. A Program can still disclose them through output or filesystem writes.

## Preserve one Run

Generate a lowercase UUID v4, then start one persistent Run:

```bash
if command -v uuidgen >/dev/null 2>&1; then
  run_id=$(uuidgen | tr '[:upper:]' '[:lower:]')
else
  run_id=$(cat /proc/sys/kernel/random/uuid)
fi

runlab run start \
  --id "$run_id" \
  --image base \
  --runtime-config config.json \
  --description 'runtime smoke' \
  --label purpose=smoke
```

On Debian and Ubuntu, `uuidgen` is available from the `uuid-runtime` package. The fallback uses the Linux kernel interface; the caller still owns the ID and must reuse it when retrying the same Run.

By default the command waits for the Engine to return. stderr streams Live Events while stdout receives one bounded terminal summary. The Run is accepted before execution and remains queryable even when execution fails. Reusing the same ID is idempotent only for the same input and metadata.

For concurrent work, detach after acceptance and inspect the persistent Run later:

```bash
runlab run start --detach --id "$run_id" --image base --runtime-config config.json
runlab run get "$run_id"
```

Detached submission returns the Run ID and a recovery command. It does not stream Program output because the independent Coordinator no longer shares the caller's terminal.

Inspect one complete record or select bounded facts across Runs:

```bash
runlab run get "$run_id"
runlab query run \
  "SELECT run_id, lifecycle, primary_exit_code FROM runs WHERE run_id = '$run_id'"
```

Discover Relation columns before querying with `runlab schema list` and `runlab schema get runs`. See `runlab docs get how-to/query-runs` for bounds and selection patterns.

After a Run is terminal, an external Method may derive a typed, persistent Observation. Discover or register its immutable Type first; built-in and external Types use the same validation, storage, and generic JSON query path. See `runlab docs get how-to/observe-runs`. This is distinct from the ephemeral Live Event stream emitted while execution is active.

## Read the Final Environment

Retrieve a known file, directory, or symlink without entering the VM data plane:

```bash
runlab filesystem get \
  --run "$run_id" \
  /artifacts/result.json \
  --output ./result.json
```

The output path must not already exist. The same command can read an Initial Image with `--image NAME` instead of `--run RUN_ID`.

When the artifact path is not known, list the bounded Final Environment changes first:

```bash
runlab filesystem changes --run "$run_id" --limit 100
```

The result distinguishes added, modified, and deleted paths. It is a derived view; the immutable Initial and Final Image descriptors remain the source facts.

## Move Images between tools

Export a Catalog Image or one Program's Final Image as a standard uncompressed OCI Image Layout archive:

```bash
runlab image export --image base --output ./base.oci.tar
runlab image export --run "$run_id" --output ./final.oci.tar
```

RunLab never overwrites the output path. External OCI tools can consume the archive; Image construction remains outside RunLab.

## Inspect and reclaim storage

Inspect VM capacity, State allocation, immutable asset references, and currently reclaimable bytes:

```bash
runlab storage status
runlab storage prune check
```

`storage prune check` is read-only. If its exact plan is acceptable, apply it with `runlab storage prune apply`. Apply requires exclusive State access and removes only unreferenced OCI blobs, unreachable Engine snapshot cache, and stale invocation staging. It does not delete Catalog entries or Run records. Apply refuses to remove any content when the retained OCI reference graph cannot be completely validated. Removing snapshot entries makes later execution timing cold-cache evidence.

Terminal Run assets can be permanently retired through a separate checked workflow. Start with `runlab docs get how-to/delete-runs`. Use `storage prune check --without-runs FILE` before deletion when the hypothetical content benefit matters; Run deletion itself does not traverse or remove OCI content.

## Handle failures as data

A command failure exits nonzero, writes no success object to stdout, and writes one `runlab.error` JSON object to stderr. Inspect `category`, `stage`, `accepted`, `run_created`, `retryable`, and `recovery`. A `null` acceptance fact means RunLab could not prove the state. When a Run may already exist, follow the supplied recovery command or use `runlab run get RUN_ID` before retrying.
