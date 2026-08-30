#!/usr/bin/env bash

set -euo pipefail

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
staging_root="$(mktemp -d "${TMPDIR:-/tmp}/runlab-package-verify.XXXXXX")"
trap 'rm -rf "$staging_root"' EXIT HUP INT TERM

export CARGO_TARGET_DIR="$staging_root/target"
cd "$repository_root"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.+-][0-9A-Za-z.-]+)?$ ]] || {
    echo "verify-packages: invalid workspace version: $version" >&2
    exit 1
}

cargo package -p run_protocol --locked --allow-dirty
cargo package \
    -p run_engine \
    --locked \
    --no-verify \
    --allow-dirty \
    --config "patch.crates-io.run_protocol.path=\"$repository_root/crates/run_protocol\""
cargo package \
    -p runlab \
    --locked \
    --no-verify \
    --allow-dirty \
    --config "patch.crates-io.run_protocol.path=\"$repository_root/crates/run_protocol\"" \
    --config "patch.crates-io.run_engine.path=\"$repository_root/crates/run_engine\""

extracted_root="$staging_root/extracted"
mkdir -p "$extracted_root"
for package_name in "run_protocol-$version" "run_engine-$version" "runlab-$version"; do
    mkdir -p "$extracted_root/$package_name"
    tar -xzf "$CARGO_TARGET_DIR/package/$package_name.crate" \
        -C "$extracted_root/$package_name" \
        --strip-components=1
done

protocol_package="$extracted_root/run_protocol-$version"
engine_package="$extracted_root/run_engine-$version"
runlab_package="$extracted_root/runlab-$version"

for package_root in "$protocol_package" "$engine_package" "$runlab_package"; do
    test -f "$package_root/Cargo.toml"
    if rg -n \
        'localhost:8787|code\.byted\.org|/Users/bytedance|BES/runlab|\.private/' \
        "$package_root"; then
        echo "public Cargo package contains private release text: $package_root" >&2
        exit 1
    fi
done

test -f "$protocol_package/LICENSE"
test -f "$protocol_package/README.md"
test -f "$engine_package/LICENSE"
test -f "$engine_package/README.md"
test -f "$runlab_package/LICENSE"
test -f "$runlab_package/README.md"
test -f "$runlab_package/CHANGELOG.md"

cargo test \
    --manifest-path "$protocol_package/Cargo.toml" \
    --all-targets \
    --locked
cargo test \
    --manifest-path "$engine_package/Cargo.toml" \
    --all-targets \
    --locked \
    --config "patch.crates-io.run_protocol.path=\"$protocol_package\""
cargo test \
    --manifest-path "$runlab_package/Cargo.toml" \
    --all-targets \
    --locked \
    --config "patch.crates-io.run_protocol.path=\"$protocol_package\"" \
    --config "patch.crates-io.run_engine.path=\"$engine_package\""

printf 'Verified normalized Cargo packages: run_protocol %s, run_engine %s, runlab %s\n' \
    "$version" "$version" "$version"
