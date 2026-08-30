#!/usr/bin/env bash

set -euo pipefail

fail() {
    echo "assemble-release: $*" >&2
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

dist_root=""
version=""
required_targets=()
while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --dist-root) dist_root="$2"; shift 2 ;;
        --version) version="${2#v}"; shift 2 ;;
        --require-target) required_targets+=("$2"); shift 2 ;;
        *) fail "unknown argument: $1" ;;
    esac
done

[[ -n "$dist_root" ]] || fail "--dist-root is required"
[[ -n "$version" ]] || fail "--version is required"
version_directory="$dist_root/v$version"
[[ -d "$version_directory" ]] || fail "missing version directory: $version_directory"

shopt -s nullglob
archives=("$version_directory"/runlab-v"$version"-*.tar.gz)
[[ "${#archives[@]}" -gt 0 ]] || fail "no release archives found"

for target in "${required_targets[@]}"; do
    [[ -f "$version_directory/runlab-v$version-$target.tar.gz" ]] ||
        fail "required target is missing: $target"
done

for archive in "${archives[@]}"; do
    checksum="$archive.sha256"
    [[ -f "$checksum" ]] || fail "missing checksum: $checksum"
    expected_hash="$(awk 'NR == 1 {print $1}' "$checksum" | tr 'A-F' 'a-f')"
    expected_name="$(awk 'NR == 1 {print $2}' "$checksum")"
    [[ "$expected_name" == "$(basename "$archive")" ]] || fail "$checksum names $expected_name"
    actual_hash="$(sha256_file "$archive" | tr 'A-F' 'a-f')"
    [[ "$actual_hash" == "$expected_hash" ]] || fail "checksum mismatch: $archive"
done

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$dist_root"
install -m 0755 "$repository_root/scripts/install.sh" "$dist_root/install.sh"
printf 'v%s\n' "$version" > "$dist_root/latest"

: > "$dist_root/SHA256SUMS"
for archive in "${archives[@]}"; do
    cat "$archive.sha256" >> "$dist_root/SHA256SUMS"
done

installer_hash="$(sha256_file "$dist_root/install.sh" | tr 'A-F' 'a-f')"
manifest="$dist_root/manifest.json"
{
    printf '{\n'
    printf '  "schema_version": 1,\n'
    printf '  "version": "%s",\n' "$version"
    printf '  "crates": {"run_protocol": "%s", "run_engine": "%s", "runlab": "%s"},\n' \
        "$version" "$version" "$version"
    printf '  "runc_version": "1.5.1",\n'
    printf '  "installer": {"path": "install.sh", "sha256": "%s"},\n' "$installer_hash"
    printf '  "artifacts": [\n'
    for index in "${!archives[@]}"; do
        archive="${archives[$index]}"
        name="$(basename "$archive")"
        target="${name#runlab-v$version-}"
        target="${target%.tar.gz}"
        hash="$(awk 'NR == 1 {print $1}' "$archive.sha256" | tr 'A-F' 'a-f')"
        comma=','
        [[ "$index" -eq "$((${#archives[@]} - 1))" ]] && comma=''
        printf '    {"target": "%s", "archive": "%s", "sha256": "%s"}%s\n' \
            "$target" "$name" "$hash" "$comma"
    done
    printf '  ]\n'
    printf '}\n'
} > "$manifest"

cp "$manifest" "$version_directory/manifest.json"
printf '%s\n' "$dist_root"
