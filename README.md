# RunLab

RunLab executes a Run from an OCI Image and preserves its input and terminal execution facts as a local Run record.

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

RunLab intentionally starts with eight State commands:

```text
runlab image import
runlab image list
runlab image get
runlab filesystem get
runlab run config generate
runlab run start
runlab run get
runlab run list
```

On macOS, RunLab also exposes explicit lifecycle commands for the fixed local Linux VM:

```text
runlab vm create
runlab vm start
runlab vm install
runlab vm stop
runlab vm status
```

`vm status` is read-only. The other lifecycle commands are idempotent and never expose a general VM shell or command executor. `vm install` verifies and atomically installs the bundled, architecture-matched Linux `runlab` and `runc`, then checks the minimal NativeEngine reference profile. Release bundles place these files beside the macOS executable as `runlab-linux-<arch>` and `runc-linux-<arch>`; development builds can select them with `RUNLAB_GUEST_BINARY` and `RUNLAB_GUEST_RUNC`.

On macOS, the ordinary `image`, `run`, and `filesystem` commands execute the same-version Linux `runlab` inside the ready VM. State remains fixed at `/var/lib/runlab`; macOS rejects `--state` and `RUNLAB_STATE` rather than treating a host path as a guest path.

Input files cross the VM boundary as explicit bytes and are checked by size and SHA-256 before use. `run start` is owned by a transient systemd service, so closing the macOS control connection does not cancel the Run; reconnect with `run get RUN_ID`. Successful `filesystem get` verifies the guest and host bytes before publishing a new macOS file. Managed-VM output transfer currently supports regular files, while native Linux `filesystem get` also supports directories and symlinks.

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

`run start` is Linux-only and requires `runc`. The caller supplies a canonical lowercase UUID v4 and an imported Image. When `--runtime-config` is omitted, RunLab uses the same bytes produced by `run config generate`:

```bash
$runlab --state "$state" run start \
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
```

`description` and repeatable `--label KEY=VALUE` store caller-provided metadata for Agent selection. Label keys and values are arbitrary strings that RunLab does not interpret. Image metadata belongs to the mutable Catalog entry and does not change the OCI digest; Run metadata is fixed when the Run is accepted and is not passed to the Engine. The combined metadata is limited to 8 KiB.

The same Run identity, semantically identical input, and identical metadata make `run start` idempotent. Reusing the identity with different input or metadata fails.

`--secret-env NAME` reads one variable from the caller environment. `--secret-file HOST_FILE=CONTAINER_PATH` reads one host file and exposes its exact bytes as a read-only regular file during execution. Secret values are part of the in-memory Run Protocol input, but RunLab does not serialize them from the Secret fields into the public Run record or Final Environment. A Program can still disclose a Secret by writing it to stdout, stderr, or its writable filesystem.

While `run start` is active, stderr emits an NDJSON observation stream beginning with `run.stream`, followed by `run.stage`, `program.stdout`, and `program.stderr` records as those observations occur. stdout remains reserved for one compact final JSON result containing the Run identity, lifecycle, execution facts, process results, final environments, and errors. It does not repeat the exact input or captured stdout/stderr. Use `run get RUN_ID` when the complete persisted Run record is required.

`--state` can be replaced by `RUNLAB_STATE`. Otherwise RunLab uses `$XDG_DATA_HOME/runlab` or `$HOME/.local/share/runlab`.

## Development checks

```bash
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo +1.95.0 check --workspace --all-targets --locked
cargo package -p run_protocol --no-verify --locked
```

Package dependents can be checked in publication order after `run_protocol 0.1.0` is available from the target registry. Linux completion additionally requires an end-to-end `run start` through the real `NativeEngine` and `runc`.
