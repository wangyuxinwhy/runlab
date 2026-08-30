#!/usr/bin/env bash

set -euo pipefail

fail() {
    echo "test-installer: $*" >&2
    exit 1
}

dist_root=""
version=""
while [[ "$#" -gt 0 ]]; do
    case "$1" in
        --dist-root) dist_root="$2"; shift 2 ;;
        --version) version="${2#v}"; shift 2 ;;
        *) fail "unknown argument: $1" ;;
    esac
done

[[ -d "$dist_root" ]] || fail "--dist-root must name an assembled release directory"
[[ -n "$version" ]] || fail "--version is required"
command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v python3 >/dev/null 2>&1 || fail "python3 is required"

case "$(uname -s):$(uname -m)" in
    Darwin:arm64|Darwin:aarch64) target="aarch64-apple-darwin"; bundle_architecture="aarch64" ;;
    Darwin:x86_64) target="x86_64-apple-darwin"; bundle_architecture="x86_64" ;;
    Linux:aarch64|Linux:arm64) target="aarch64-unknown-linux-gnu"; bundle_architecture="" ;;
    Linux:x86_64|Linux:amd64) target="x86_64-unknown-linux-gnu"; bundle_architecture="" ;;
    *) fail "unsupported test platform: $(uname -s) $(uname -m)" ;;
esac

archive_name="runlab-v$version-$target.tar.gz"
archive="$dist_root/v$version/$archive_name"
[[ -f "$archive" ]] || fail "current-platform archive is missing: $archive"
[[ -f "$archive.sha256" ]] || fail "current-platform checksum is missing"
[[ -f "$dist_root/latest" ]] || fail "assembled latest pointer is missing"

test_root="$(mktemp -d "${TMPDIR:-/tmp}/runlab-installer-test.XXXXXX")"
server_pid=""
cleanup() {
    if [[ -n "$server_pid" ]]; then
        kill "$server_pid" >/dev/null 2>&1 || true
        wait "$server_pid" >/dev/null 2>&1 || true
    fi
    rm -rf "$test_root"
}
trap cleanup EXIT HUP INT TERM

serve_root="$test_root/server"
mkdir -p "$serve_root/download/v$version" "$serve_root/latest/download"
install -m 0644 "$archive" "$serve_root/download/v$version/$archive_name"
install -m 0644 "$archive.sha256" "$serve_root/download/v$version/$archive_name.sha256"
install -m 0644 "$dist_root/latest" "$serve_root/latest/download/latest"

port="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()')"
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$serve_root" \
    >"$test_root/http.log" 2>&1 &
server_pid="$!"
base_url="http://127.0.0.1:$port"
for _ in {1..50}; do
    if curl --fail --silent "$base_url/latest/download/latest" >/dev/null; then
        break
    fi
    sleep 0.1
done
curl --fail --silent "$base_url/latest/download/latest" >/dev/null || fail "local release server did not start"

bin_directory="$test_root/install/bin"
doc_directory="$test_root/install/share/doc/runlab"
RUNLAB_DIST_BASE_URL="$base_url" sh "$dist_root/install.sh" \
    --allow-http \
    --bin-dir "$bin_directory" \
    --doc-dir "$doc_directory"

[[ "$("$bin_directory/runlab" --version)" == "runlab $version" ]] || fail "installed binary version mismatch"
[[ -f "$doc_directory/LICENSE" ]] || fail "RunLab license was not installed"
[[ -f "$doc_directory/RUNC-LICENSE" ]] || fail "runc license was not installed"
[[ -f "$doc_directory/RUNC-NOTICE" ]] || fail "runc notice was not installed"
[[ -f "$doc_directory/THIRD_PARTY_NOTICES.md" ]] || fail "third-party notices were not installed"
if [[ -n "$bundle_architecture" ]]; then
    [[ -x "$bin_directory/runlab-linux-$bundle_architecture" ]] || fail "managed-VM RunLab binary was not installed"
    [[ -x "$bin_directory/runc-linux-$bundle_architecture" ]] || fail "managed-VM runc was not installed"
else
    [[ -x "$bin_directory/runlab-runc" ]] || fail "private Linux runc was not installed"
    runc_output="$("$bin_directory/runlab-runc" --version)"
    [[ "${runc_output%%$'\n'*}" == "runc version 1.5.1" ]] ||
        fail "private Linux runc version mismatch"
    grep -Fx 'spec: 1.3.0' <<<"$runc_output" >/dev/null ||
        fail "private Linux runc OCI specification mismatch"
fi

printf '%064d  %s\n' 0 "$archive_name" > "$serve_root/download/v$version/$archive_name.sha256"
if RUNLAB_DIST_BASE_URL="$base_url" sh "$dist_root/install.sh" \
    --allow-http \
    --version "$version" \
    --bin-dir "$test_root/rejected/bin" \
    --doc-dir "$test_root/rejected/doc" \
    >"$test_root/rejected.stdout" 2>"$test_root/rejected.stderr"; then
    fail "installer accepted a bad archive checksum"
fi
grep -q 'SHA-256 mismatch' "$test_root/rejected.stderr" || fail "checksum rejection was not explicit"
[[ ! -e "$test_root/rejected/bin/runlab" ]] || fail "checksum failure changed the installation"

printf 'Verified %s installer, latest resolution, payload, and checksum rejection\n' "$target"
