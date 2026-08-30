#!/usr/bin/env bash

set -euo pipefail

fail() {
    echo "package-release: $*" >&2
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

binary=""
target=""
output_root=""
version=""
guest_binary=""
runc_binary=""
runc_license=""
runc_notice=""

while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --binary) binary="$2"; shift 2 ;;
        --target) target="$2"; shift 2 ;;
        --output-root) output_root="$2"; shift 2 ;;
        --version) version="${2#v}"; shift 2 ;;
        --guest-binary) guest_binary="$2"; shift 2 ;;
        --runc-binary) runc_binary="$2"; shift 2 ;;
        --runc-license) runc_license="$2"; shift 2 ;;
        --runc-notice) runc_notice="$2"; shift 2 ;;
        *) fail "unknown argument: $1" ;;
    esac
done

[[ -x "$binary" ]] || fail "--binary must name an executable file"
[[ -n "$target" ]] || fail "--target is required"
[[ -n "$output_root" ]] || fail "--output-root is required"
[[ -n "$version" ]] || fail "--version is required"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.+-][0-9A-Za-z.-]+)?$ ]] || fail "invalid version: $version"

reported_version="$("$binary" --version)"
[[ "$reported_version" == "runlab $version" ]] ||
    fail "binary reports '$reported_version', expected 'runlab $version'"

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
staging_root="$(mktemp -d "${TMPDIR:-/tmp}/runlab-release-package.XXXXXX")"
trap 'rm -rf "$staging_root"' EXIT HUP INT TERM

install -m 0755 "$binary" "$staging_root/runlab"
install -m 0644 "$repository_root/LICENSE" "$staging_root/LICENSE"
install -m 0644 "$repository_root/THIRD_PARTY_NOTICES.md" "$staging_root/THIRD_PARTY_NOTICES.md"

archive_entries=(LICENSE THIRD_PARTY_NOTICES.md runlab)
case "$target" in
    aarch64-apple-darwin) bundle_architecture="aarch64" ;;
    x86_64-apple-darwin) bundle_architecture="x86_64" ;;
    aarch64-unknown-linux-gnu|x86_64-unknown-linux-gnu) bundle_architecture="" ;;
    *) fail "unsupported release target: $target" ;;
esac

if [[ -n "$bundle_architecture" ]]; then
    for required_file in "$guest_binary" "$runc_binary" "$runc_license" "$runc_notice"; do
        [[ -f "$required_file" ]] || fail "macOS bundle input is missing: $required_file"
    done
    install -m 0755 "$guest_binary" "$staging_root/runlab-linux-$bundle_architecture"
    install -m 0755 "$runc_binary" "$staging_root/runc-linux-$bundle_architecture"
    install -m 0644 "$runc_license" "$staging_root/RUNC-LICENSE"
    install -m 0644 "$runc_notice" "$staging_root/RUNC-NOTICE"
    archive_entries+=(RUNC-LICENSE RUNC-NOTICE "runlab-linux-$bundle_architecture" "runc-linux-$bundle_architecture")
else
    [[ -z "$guest_binary$runc_binary$runc_license$runc_notice" ]] ||
        fail "Linux release archives do not accept managed-VM bundle inputs"
fi

version_directory="$output_root/v$version"
archive_name="runlab-v$version-$target.tar.gz"
archive="$version_directory/$archive_name"
mkdir -p "$version_directory"
rm -f "$archive" "$archive.sha256"
COPYFILE_DISABLE=1 tar -C "$staging_root" -cf - "${archive_entries[@]}" | gzip -n > "$archive"
archive_hash="$(sha256_file "$archive" | tr 'A-F' 'a-f')"
printf '%s  %s\n' "$archive_hash" "$archive_name" > "$archive.sha256"
printf '%s\n' "$archive"
