#!/usr/bin/env bash

set -euo pipefail

fail() {
    echo "fetch-runc: $*" >&2
    exit 1
}

sha256_file() {
    local file_path="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file_path" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file_path" | awk '{print $1}'
    else
        openssl dgst -sha256 "$file_path" | awk '{print $NF}'
    fi
}

architecture=""
output_directory=""
while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --architecture) architecture="$2"; shift 2 ;;
        --output) output_directory="$2"; shift 2 ;;
        *) fail "unknown argument: $1" ;;
    esac
done

[[ -n "$architecture" ]] || fail "--architecture is required"
[[ -n "$output_directory" ]] || fail "--output is required"

runc_version="1.5.1"
case "$architecture" in
    aarch64)
        upstream_architecture="arm64"
        expected_binary="ca70e7dbd6616ca782a59b5d3ac86909123fdaa9fa3f89dcf29051c70eee7ce9"
        ;;
    x86_64)
        upstream_architecture="amd64"
        expected_binary="177df879d50c913eb205e898d5c1c05a18f574053c0ce5524c471208eaf06f6f"
        ;;
    *) fail "unsupported architecture: $architecture" ;;
esac

expected_license="552a739c3b25792263f731542238b92f6f8d07e9a488eae27e6c4690038a8243"
expected_notice="e94a9789f41c5c3d6f74212571e6f44367de88269f1ce9c32f26c1b67eec6e7f"
mkdir -p "$output_directory"
binary="$output_directory/runc-linux-$architecture"
license="$output_directory/RUNC-LICENSE"
notice="$output_directory/RUNC-NOTICE"

curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
    --output "$binary" \
    "https://github.com/opencontainers/runc/releases/download/v$runc_version/runc.$upstream_architecture"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
    --output "$license" \
    "https://raw.githubusercontent.com/opencontainers/runc/v$runc_version/LICENSE"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
    --output "$notice" \
    "https://raw.githubusercontent.com/opencontainers/runc/v$runc_version/NOTICE"

[[ "$(sha256_file "$binary" | tr 'A-F' 'a-f')" == "$expected_binary" ]] || fail "runc binary checksum mismatch"
[[ "$(sha256_file "$license" | tr 'A-F' 'a-f')" == "$expected_license" ]] || fail "runc LICENSE checksum mismatch"
[[ "$(sha256_file "$notice" | tr 'A-F' 'a-f')" == "$expected_notice" ]] || fail "runc NOTICE checksum mismatch"
chmod 0755 "$binary"
printf '%s\n' "$output_directory"
