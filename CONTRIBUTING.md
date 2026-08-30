# Contributing

RunLab is a Rust 2024 workspace with exactly three packages: `run_protocol`, `run_engine`, and the `runlab` binary. Preserve the dependency direction `runlab -> run_engine -> run_protocol`, with `runlab` also allowed to depend directly on `run_protocol`.

Before submitting a change, run:

```bash
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo +1.95.0 check --workspace --all-targets --locked
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --document-private-items --locked
npm ci
npm run docs:check
scripts/verify-packages.sh
```

Changes to Linux execution mechanics also require the real-`runc` gate documented in `RELEASING.md`. Tests must express intended behavior and evidence boundaries; do not freeze a defect merely because it is current behavior.

Files under `docs/design/generated/` are generated design snapshots. Do not edit them directly. Operational guides and references are maintained with the code and must match the current CLI and public schema.
