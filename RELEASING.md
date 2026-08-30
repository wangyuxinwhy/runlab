# Releasing RunLab

Publishing source, publishing three Cargo packages, deploying documentation, creating a Git tag, and publishing a GitHub Release are separate state changes. `0.1.0` is the first public RunLab release; the existing database schema remains version 6 because it protects real persisted State.

## Prepare a release candidate

1. Confirm the worktree is clean and `Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`, `runlab --version`, bundled docs, the managed-VM handshake, and the release manifest agree.
2. Run the complete repository verification:

   ```bash
   cargo fmt --all -- --check
   cargo test --workspace --all-targets --locked
   cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
   cargo +1.95.0 check --workspace --all-targets --locked
   RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --document-private-items --locked
   npm ci
   npm run docs:check
   scripts/verify-packages.sh
   scripts/test-installer.sh
   ```

3. On the managed Linux data plane, inspect disk capacity and run `scripts/verify-linux.sh` as root with the small, digest-pinned Alpine OCI Image Layout used by the real-`runc` fixtures. Do not substitute an arbitrary Catalog Image: large root filesystems materially change Final Environment capture and filesystem-sync cost. Preserve at least 20% of the 32 GiB planning budget and remove only this release candidate's temporary build paths.
4. Install the macOS release candidate into the fixed Managed VM, execute a deterministic Run through real `runc`, and verify interruption, stdout, stderr, Final Environment retrieval, and zero invocation residue.
5. Run the private release regression against the exact candidate artifacts. Do not copy its trace, tasks, Rubrics, Runs, scores, or private measurements into the public repository.

## Build a Draft GitHub Release

Merge the release PR, then push one annotated `vVERSION` tag. The Release workflow verifies the tag against the root package and Changelog, builds native Linux and macOS binaries, obtains digest-pinned `runc` 1.5.1 binaries, assembles platform archives, and creates or updates a Draft Release.

The workflow is retry-safe: it edits an existing Draft and replaces same-name assets instead of assuming a previous attempt made no external changes. Release notes come from the exact Changelog section rather than a tag-message heuristic.

Inspect and install every Draft artifact before publishing Cargo packages.

## Publish Cargo packages

The packages are irreversible, ordered registry publications:

```text
run_protocol 0.1.0
        ↓ wait until crates.io resolves the exact version
run_engine 0.1.0
        ↓ wait until crates.io resolves the exact version
runlab 0.1.0
```

The first publication of each crate requires a short-lived crates.io API token because Trusted Publishing cannot be configured before a crate exists. Publish the bootstrap release from a verified checkout with `scripts/publish-crates.sh`, or place that narrowly scoped token in the GitHub `release` environment as `CARGO_REGISTRY_TOKEN` and manually dispatch `publish.yml` in `bootstrap-token` mode. After all three names exist, configure each crate's Trusted Publisher for repository `wangyuxinwhy/runlab`, workflow `publish.yml`, and environment `release`; remove the bootstrap token, and use `trusted-publisher` mode thereafter.

Never move or reuse a published tag. A defective Cargo version is yanked and replaced by a new patch version; a defective GitHub asset is replaced only while the Release remains Draft.

## Publish and verify

After crates.io resolves all three exact versions, publish the Draft GitHub Release and verify from clean environments:

```bash
cargo install runlab --version VERSION --locked
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/wangyuxinwhy/runlab/releases/latest/download/install.sh \
  -o install-runlab.sh
sh install-runlab.sh
```

Verify the GitHub Pages HTML, `llms.txt`, `llms-full.txt`, every link in `llms.txt`, and at least one generated design Markdown page. Keep the previous distribution surface until all external checks succeed.
