#!/bin/sh
# Type-check, lint, and test the Linux-only code paths from a non-Linux host.
#
# Roughly a fifth of this crate (native_backend, native_cgroup, native_fs,
# native_reconcile, native_resolver, materialize, read_only_file) is behind
# `#[cfg(target_os = "linux")]` and is never compiled by a plain `cargo` run on
# macOS. Without this script those paths reach CI unchecked.
#
# Usage: scripts/linux-check.sh [cargo arguments...]
#   scripts/linux-check.sh                       # clippy + test
#   scripts/linux-check.sh cargo clippy --all-targets
set -eu

IMAGE="rust:$(sed -n 's/^channel = "\(.*\)"/\1/p' rust-toolchain.toml)"
REPO="$(cd "$(dirname "$0")/.." && pwd)"

if [ "$#" -eq 0 ]; then
    set -- sh -c 'cargo clippy --all-targets && cargo test --all-targets'
fi

exec docker run --rm \
    -v "$REPO":/workspace -w /workspace \
    -v runlab-linux-cargo-registry:/usr/local/cargo/registry \
    -v runlab-linux-rustup:/usr/local/rustup \
    -v runlab-linux-target:/workspace/target-linux \
    -e CARGO_TARGET_DIR=/workspace/target-linux \
    "$IMAGE" "$@"
