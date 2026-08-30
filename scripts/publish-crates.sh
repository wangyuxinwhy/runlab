#!/usr/bin/env bash

set -euo pipefail

fail() {
    echo "publish-crates: $*" >&2
    exit 1
}

repository_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repository_root/Cargo.toml" | head -n 1)"
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.+-][0-9A-Za-z.-]+)?$ ]] || fail "invalid workspace version: $version"
command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v jq >/dev/null 2>&1 || fail "jq is required"

status_root="$(mktemp -d "${TMPDIR:-/tmp}/runlab-crates-publish.XXXXXX")"
trap 'rm -rf "$status_root"' EXIT HUP INT TERM

registry_status() {
    local crate_name="$1"
    local response="$status_root/$crate_name.json"
    local status
    status="$(curl --silent --show-error --location \
        --output "$response" \
        --write-out '%{http_code}' \
        --user-agent 'runlab-release/0.1 (+https://github.com/wangyuxinwhy/runlab)' \
        "https://crates.io/api/v1/crates/$crate_name/$version")"
    case "$status" in
        200)
            [[ "$(jq -r '.version.num' "$response")" == "$version" ]] || fail "crates.io returned a different $crate_name version"
            [[ "$(jq -r '.version.yanked' "$response")" == "false" ]] || fail "$crate_name $version is yanked"
            [[ "$(jq -r '.version.repository' "$response")" == "https://github.com/wangyuxinwhy/runlab" ]] ||
                fail "$crate_name $version exists but belongs to another repository"
            return 0
            ;;
        404) return 1 ;;
        *) fail "crates.io returned HTTP $status for $crate_name $version" ;;
    esac
}

wait_until_resolvable() {
    local crate_name="$1"
    local attempt
    for attempt in {1..90}; do
        if registry_status "$crate_name" && cargo info --registry crates-io "$crate_name@$version" >/dev/null 2>&1; then
            return 0
        fi
        sleep 10
    done
    fail "$crate_name $version did not become resolvable within 15 minutes"
}

cd "$repository_root"
for crate_name in run_protocol run_engine runlab; do
    if registry_status "$crate_name"; then
        echo "$crate_name $version is already published by this repository; verifying registry resolution"
    else
        cargo publish -p "$crate_name" --locked
    fi
    wait_until_resolvable "$crate_name"
done

printf 'Published and resolved run_protocol, run_engine, and runlab %s\n' "$version"
