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

Ordinary State commands are not forwarded to the VM yet.

Image building and registry transport belong to external OCI tools. `image import` accepts a standard OCI Image Layout directory or an uncompressed tar archive containing one Image Manifest.

```bash
cargo build --release --locked
runlab=./target/release/runlab
state=./runlab-state

$runlab --state "$state" image import ./image-layout --name agent-base
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
  --image agent-base

$runlab --state "$state" run start \
  --id "$(uuidgen | tr '[:upper:]' '[:lower:]')" \
  --image agent-base \
  --runtime-config ./config.json \
  --network egress

$runlab --state "$state" run list
$runlab --state "$state" run get <run-id>
```

The same Run identity and semantically identical input make `run start` idempotent. Reusing the identity with different input fails.

`run start` writes a compact JSON result containing the Run identity, lifecycle, execution facts, process results, final environments, and errors. It does not repeat the exact input or captured stdout/stderr. Use `run get RUN_ID` only when the complete persisted Run record is required.

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
