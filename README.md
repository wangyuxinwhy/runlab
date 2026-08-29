# RunLab

RunLab executes programs from OCI Images. `run start` preserves an execution as a local Run asset; `exec` performs the same protocol invocation for immediate observation without creating a Run.

The repository is a Rust workspace with exactly three packages:

```text
runlab -> run_engine -> run_protocol
   |                       ^
   +-----------------------+
```

- `run_protocol` defines the execution input, output, errors, and structural invariants. It does not own identities, records, persistence, or execution.
- `run_engine` executes the protocol. Its current implementation is the Linux `NativeEngine` backed by `runc`.
- `runlab` owns the local OCI content store, Image catalog, Run records, and command-line product surface.

Settled design is maintained in the [RunLab Agent Wiki](http://localhost:8787/app/pages/runlab-index--nw). This README describes only the implemented repository surface.

## Commands

RunLab's State-dependent execution and read commands are:

```text
runlab image import
runlab image list
runlab image get
runlab image export
runlab filesystem get
runlab filesystem changes
runlab exec
runlab run config generate
runlab run start
runlab run cancel
runlab run get
runlab run list
runlab schema list|get
runlab query run
runlab storage status
runlab storage prune check|apply
```

Version-matched operational guidance is bundled with the binary and does not open State or start the Managed VM:

```text
runlab docs list
runlab docs get how-to/build-images
runlab docs get how-to/query-runs
```

`docs get` writes Markdown by default; `--output json` returns the same document in a compact JSON envelope. The bundled guides cover standard OCI Image authoring and bounded read-only Run queries.

On macOS, RunLab also exposes explicit lifecycle commands for the fixed local Linux VM:

```text
runlab vm create
runlab vm start
runlab vm install
runlab vm stop
runlab vm status
```

`vm status` is read-only and reports configured capacity plus Guest-used and available bytes. The other lifecycle commands are idempotent and never expose a general VM shell or command executor. `vm install` verifies and atomically installs the bundled, architecture-matched Linux `runlab` and `runc`, then checks the minimal NativeEngine reference profile. Release bundles place these files beside the macOS executable as `runlab-linux-<arch>` and `runc-linux-<arch>`; development builds can select them with `RUNLAB_GUEST_BINARY` and `RUNLAB_GUEST_RUNC`.

On macOS, the ordinary `exec`, `image`, `run`, `filesystem`, `schema`, and `query` commands execute the same-version Linux `runlab` inside the ready VM. State remains fixed at `/var/lib/runlab`; macOS rejects `--state` and `RUNLAB_STATE` rather than treating a host path as a guest path.

Input files cross the VM boundary as explicit bytes and are checked by size and SHA-256 before use. `run start` is owned by a transient systemd service, so an unexpected macOS control-connection loss does not cancel the Run; reconnect with `run get RUN_ID` or explicitly request cancellation with `run cancel RUN_ID`. A foreground `SIGINT` or `SIGTERM` is forwarded to the exact Guest invocation and RunLab waits for its bounded termination result. `filesystem get` transfers a file, directory, or symlink through one checked archive and publishes it to a new macOS path without overwriting an existing node.

An explicit macOS Runtime Configuration can bind-mount a local regular file or directory when the mount contains the OCI `ro` option. RunLab stages the source into the VM and maps it back to the unchanged absolute source path inside the execution unit's private mount namespace, so the exact caller JSON remains the Run Protocol input and persisted Run fact. Writable Host bind mounts are rejected before acceptance because silently discarding their writes would violate OCI bind-mount semantics.

Command failures write one JSON object to stderr with `kind: "runlab.error"`, a stable category and stage, optional Run identity, explicit acceptance and creation facts when known, retryability, and an optional recovery command. A `null` acceptance fact means RunLab could not prove the state, not `false`. Program stdout/stderr observations remain NDJSON events and are not converted into command errors.

Image building and registry transport belong to external OCI tools. `image import` accepts a standard OCI Image Layout directory or an uncompressed tar archive containing one Image Manifest.

```bash
cargo build --release --locked
runlab=./target/release/runlab
state=./runlab-state

$runlab --state "$state" image import ./image-layout \
  --name agent-base \
  --description "Python 3.12 + uv; no Agent installed" \
  --label runtime=python \
  --label package_manager=uv
$runlab --state "$state" image list
$runlab --state "$state" image get agent-base
$runlab --state "$state" image export --image agent-base --output ./agent-base.oci.tar
$runlab --state "$state" filesystem get --image agent-base /workspace/result.patch --output ./result.patch
```

`filesystem get` can also resolve the Final Environment of one Run Program. It copies a regular file, directory, or symlink to a new local path and never merges with or overwrites an existing path:

```bash
$runlab --state "$state" filesystem get \
  --run <run-id> \
  --program primary \
  /artifacts/solution.patch \
  --output ./solution.patch
```

`run config generate` writes a complete OCI Runtime Configuration 1.3.0 to stdout. It combines the Image Config execution defaults with RunLab's fixed Linux execution scaffold, so ordinary JSON tools can inspect or change the result without a RunLab-specific configuration language:

```bash
$runlab --state "$state" run config generate --image agent-base >base-config.json
jq '.process.args = ["python", "-m", "agent"]' base-config.json >config.json
```

The generated configuration always creates a new network namespace and is compatible with both Run network modes. Network policy is not an OCI Runtime Configuration field.

`exec` is the non-persistent execution surface. It resolves the same Image, Runtime Configuration, stdin, Secrets, timeout, and network controls as `run start`, but it has no `run_id` or metadata, does not insert a Run record, and asks the Engine not to capture a Final Environment. Program and external side effects are real, so it is useful for preflight inspection and Observation but is not a dry run:

```bash
$runlab --state "$state" exec \
  --image agent-base \
  --runtime-config ./config.json \
  --stdin ./prompt.txt \
  --network isolated >execution.json
```

stderr uses the same NDJSON observation event shapes as `run start`, beginning with `{"kind":"run.stream","schema_version":1,"run_id":null}`. stdout contains the complete bounded `RunOutput` or `EngineError`, including retained stdout and stderr, because there is no later `run get`. The Final Environment field is explicitly `not_requested`.

`run start` is Linux-only and requires `runc`. The caller supplies a canonical lowercase UUID v4 and an imported Image. When `--runtime-config` is omitted, RunLab uses the same bytes produced by `run config generate`:

```bash
$runlab --state "$state" run start \
  --detach \
  --id "$(uuidgen | tr '[:upper:]' '[:lower:]')" \
  --image agent-base \
  --description "SWE-bench django__django-11099 with pi" \
  --label suite=swe-bench \
  --label task=django__django-11099

$runlab --state "$state" run start \
  --id "$(uuidgen | tr '[:upper:]' '[:lower:]')" \
  --image agent-base \
  --runtime-config ./config.json \
  --secret-env API_KEY \
  --secret-file ./auth.json=/run/secrets/auth.json \
  --network egress

$runlab --state "$state" run list
$runlab --state "$state" run get <run-id>
$runlab --state "$state" run cancel <run-id>
$runlab --state "$state" schema get runs --compact
$runlab --state "$state" query run \
  "SELECT run_id, initial_image_name, lifecycle FROM runs ORDER BY accepted_at DESC LIMIT 10"
```

`description` and repeatable `--label KEY=VALUE` store caller-provided metadata for Agent selection. Label keys and values are arbitrary strings that RunLab does not interpret. Image metadata belongs to the mutable Catalog entry and does not change the OCI digest; Run metadata and the Catalog name selected for the Initial Image are fixed when the Run is accepted and are not passed to the Engine. The combined metadata is limited to 8 KiB.

The same Run identity, semantically identical input, and identical accepted caller facts make `run start` idempotent. Reusing the identity with a different input, Initial Image name, or metadata fails.

Without `--detach`, `run start` streams observations and waits for its terminal summary. With `--detach`, RunLab waits only until acceptance is observable, then returns the Run ID and recovery command while an independent Coordinator continues. Use `run get` or `query run` for subsequent observation; detached submission deliberately has no terminal-bound Program stream.

`run cancel RUN_ID` idempotently persists a cancellation request for a non-terminal Run. Its success confirms that the request is stored, not that the Program has already stopped. `run get RUN_ID` exposes `cancellation_requested_at`; the final `RunOutput` remains the source of truth for `cancelled`, stop actions, and the process result. A terminal Run is returned unchanged.

`--secret-env NAME` reads one variable from the caller environment. `--secret-file HOST_FILE=CONTAINER_PATH` reads one host file and exposes its exact bytes as a read-only regular file during execution. Secret values are part of the in-memory Run Protocol input, but RunLab does not serialize them from the Secret fields into the public Run record or Final Environment. A Program can still disclose a Secret by writing it to stdout, stderr, or its writable filesystem.

While `run start` is active, stderr emits an NDJSON observation stream beginning with `run.stream`, followed by `run.stage`, `program.stdout`, and `program.stderr` records as those observations occur. Its stdout remains reserved for one compact final JSON result containing the Run identity, lifecycle, execution facts, process results, final environments, and errors. It does not repeat the exact input or captured stdout/stderr. Use `run get RUN_ID` when the complete persisted Run record is required.

For a foreground `exec`, `SIGINT` and `SIGTERM` cancel the current synchronous Engine invocation; no temporary public identity or follow-up command is created. `--state` can be replaced by `RUNLAB_STATE`. Otherwise RunLab uses `$XDG_DATA_HOME/runlab` or `$HOME/.local/share/runlab`.

`storage status` reports filesystem capacity, State component allocation, immutable asset references, missing referenced content, and safely reclaimable bytes. `storage prune check` returns the same bounded deletion plan without mutation. `storage prune apply` requires exclusive State access and removes only unreferenced OCI blobs, unreachable snapshot cache entries, and stale invocation staging; it never deletes Catalog entries, Run records, or their referenced OCI content.

`assets.active_runs` counts Runs for which no terminal completion has been published. It is a persistent lifecycle count, not proof that a Coordinator or Program process is currently alive. `run reconcile RUN_ID` consults the private execution journal: it can publish a durably staged Engine result, or publish `interrupted` when the recorded owner is dead and the journal proves the Engine was never started. Once the Engine has started, the Run remains accepted with `evidence_incomplete` unless safe resource cleanup or an Engine result has been proved.

`image export` writes a Catalog Image or Run Final Image as a standard uncompressed OCI Image Layout archive. It verifies every exported blob while streaming, publishes atomically, and never overwrites an existing output path. RunLab still does not build Images or own registry transport.

## Development checks

```bash
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo +1.95.0 check --workspace --all-targets --locked
cargo package -p run_protocol --no-verify --locked
```

Package dependents can be checked in publication order after `run_protocol 0.1.0` is available from the target registry. Linux completion additionally runs `scripts/verify-linux.sh` as root with `RUNLAB_NATIVE_E2E_OCI_LAYOUT` set to a deterministic OCI fixture; the gate fails unless all six opt-in real-`runc` scenarios actually run.
