#!/bin/sh

set -eu

RUNLAB_DIST_BASE_URL_DEFAULT="${RUNLAB_DIST_BASE_URL_DEFAULT:-https://github.com/wangyuxinwhy/runlab/releases}"

fail() {
    echo "runlab installer: $*" >&2
    exit 1
}

sha256_file() {
    file_path="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file_path" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file_path" | awk '{print $1}'
    else
        openssl dgst -sha256 "$file_path" | awk '{print $NF}'
    fi
}

fetch() {
    source_url="$1"
    destination="$2"
    command -v curl >/dev/null 2>&1 || fail "curl is required"
    if [ "$allow_http" -eq 1 ]; then
        curl --proto '=http,https' --proto-redir '=http,https' --fail --location --silent --show-error --output "$destination" "$source_url"
    else
        curl --proto '=https' --proto-redir '=https' --tlsv1.2 --fail --location --silent --show-error --output "$destination" "$source_url"
    fi
}

version="latest"
base_url="${RUNLAB_DIST_BASE_URL:-$RUNLAB_DIST_BASE_URL_DEFAULT}"
bin_directory="${RUNLAB_BIN_DIR:-${HOME:?HOME is not set}/.local/bin}"
doc_directory="${RUNLAB_DOC_DIR:-${HOME:?HOME is not set}/.local/share/doc/runlab}"
dry_run=0
allow_http=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version) version="$2"; shift 2 ;;
        --bin-dir) bin_directory="$2"; shift 2 ;;
        --doc-dir) doc_directory="$2"; shift 2 ;;
        --base-url) base_url="$2"; shift 2 ;;
        --dry-run) dry_run=1; shift ;;
        --allow-http) allow_http=1; shift ;;
        -h|--help)
            echo "usage: install.sh [--version VERSION] [--bin-dir DIR] [--doc-dir DIR] [--base-url URL] [--dry-run]"
            exit 0
            ;;
        *) fail "unknown argument: $1" ;;
    esac
done

case "$bin_directory:$doc_directory" in
    /*:/*) ;;
    *) fail "installation directories must be absolute" ;;
esac
base_url="${base_url%/}"
case "$base_url" in
    https://*) ;;
    http://*) [ "$allow_http" -eq 1 ] || fail "plain HTTP requires --allow-http" ;;
    *) fail "--base-url must use HTTPS" ;;
esac

operating_system="$(uname -s)"
machine_architecture="$(uname -m)"
bundle_architecture=""
case "$operating_system:$machine_architecture" in
    Darwin:arm64|Darwin:aarch64) target="aarch64-apple-darwin"; bundle_architecture="aarch64" ;;
    Darwin:x86_64) target="x86_64-apple-darwin"; bundle_architecture="x86_64" ;;
    Linux:aarch64|Linux:arm64) target="aarch64-unknown-linux-gnu" ;;
    Linux:x86_64|Linux:amd64) target="x86_64-unknown-linux-gnu" ;;
    *) fail "unsupported platform: $operating_system $machine_architecture" ;;
esac

if [ "$dry_run" -eq 1 ]; then
    echo "version: $version"
    echo "target: $target"
    echo "binary: $bin_directory/runlab"
    [ -z "$bundle_architecture" ] || echo "managed VM bundle: runlab-linux-$bundle_architecture, runc-linux-$bundle_architecture"
    exit 0
fi

temporary_directory="$(mktemp -d "${TMPDIR:-/tmp}/runlab-install.XXXXXX")"
trap 'rm -rf "$temporary_directory"' EXIT HUP INT TERM
if [ "$version" = "latest" ]; then
    fetch "$base_url/latest/download/latest" "$temporary_directory/latest"
    version="$(tr -d '\r\n' < "$temporary_directory/latest")"
fi
version="${version#v}"
printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([.+-][0-9A-Za-z.-]+)?$' || fail "invalid release version: $version"

archive_name="runlab-v$version-$target.tar.gz"
archive="$temporary_directory/$archive_name"
checksum="$archive.sha256"
fetch "$base_url/download/v$version/$archive_name" "$archive"
fetch "$base_url/download/v$version/$archive_name.sha256" "$checksum"

expected_hash="$(awk 'NR == 1 {print $1}' "$checksum" | tr 'A-F' 'a-f')"
expected_name="$(awk 'NR == 1 {print $2}' "$checksum")"
[ "$expected_name" = "$archive_name" ] || fail "checksum names $expected_name"
[ "$(sha256_file "$archive" | tr 'A-F' 'a-f')" = "$expected_hash" ] || fail "SHA-256 mismatch"

if [ -n "$bundle_architecture" ]; then
    expected_entries="$(printf '%s\n' LICENSE RUNC-LICENSE RUNC-NOTICE THIRD_PARTY_NOTICES.md runlab "runlab-linux-$bundle_architecture" "runc-linux-$bundle_architecture" | LC_ALL=C sort)"
else
    expected_entries="$(printf '%s\n' LICENSE RUNC-LICENSE RUNC-NOTICE THIRD_PARTY_NOTICES.md runlab runlab-runc | LC_ALL=C sort)"
fi
actual_entries="$(tar -tzf "$archive" | LC_ALL=C sort)"
[ "$actual_entries" = "$expected_entries" ] || fail "archive contains unexpected paths"
tar -xzf "$archive" -C "$temporary_directory"
[ "$("$temporary_directory/runlab" --version)" = "runlab $version" ] || fail "downloaded binary reports the wrong version"

mkdir -p "$bin_directory" "$doc_directory"
if [ -n "$bundle_architecture" ]; then
    install -m 0755 "$temporary_directory/runlab-linux-$bundle_architecture" "$bin_directory/.runlab-linux-$bundle_architecture.install.$$"
    mv -f "$bin_directory/.runlab-linux-$bundle_architecture.install.$$" "$bin_directory/runlab-linux-$bundle_architecture"
    install -m 0755 "$temporary_directory/runc-linux-$bundle_architecture" "$bin_directory/.runc-linux-$bundle_architecture.install.$$"
    mv -f "$bin_directory/.runc-linux-$bundle_architecture.install.$$" "$bin_directory/runc-linux-$bundle_architecture"
else
    install -m 0755 "$temporary_directory/runlab-runc" "$bin_directory/.runlab-runc.install.$$"
    mv -f "$bin_directory/.runlab-runc.install.$$" "$bin_directory/runlab-runc"
fi
install -m 0644 "$temporary_directory/LICENSE" "$doc_directory/LICENSE"
install -m 0644 "$temporary_directory/RUNC-LICENSE" "$doc_directory/RUNC-LICENSE"
install -m 0644 "$temporary_directory/RUNC-NOTICE" "$doc_directory/RUNC-NOTICE"
install -m 0644 "$temporary_directory/THIRD_PARTY_NOTICES.md" "$doc_directory/THIRD_PARTY_NOTICES.md"
install -m 0755 "$temporary_directory/runlab" "$bin_directory/.runlab.install.$$"
mv -f "$bin_directory/.runlab.install.$$" "$bin_directory/runlab"

echo "installed runlab $version to $bin_directory/runlab"
case ":$PATH:" in
    *":$bin_directory:"*) ;;
    *) echo "add $bin_directory to PATH before running runlab" >&2 ;;
esac
