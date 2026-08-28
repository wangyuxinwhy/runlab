#!/bin/bash
set -euo pipefail

fail() {
    printf 'codex smoke failed: %s\n' "$1" >&2
    exit 1
}

test "$(id -u)" = 1000 || fail "uid is not 1000"
test "$(id -g)" = 1000 || fail "gid is not 1000"
test "$PWD" = /workspace || fail "working directory is not /workspace"

test "$(command -v codex)" = /usr/local/bin/codex || fail "codex is not on PATH"
test "$(readlink /usr/local/bin/codex)" = /opt/agents/codex/bin/codex \
    || fail "codex does not resolve to the isolated install"
test "$(codex --version)" = "codex-cli 0.150.1" || fail "unexpected codex version"
test ! -w /opt/agents/codex || fail "/opt/agents/codex is writable"
test -w /home/agent/.codex || fail "codex state directory is not writable"

codex exec --help | grep -q -- '--dangerously-bypass-approvals-and-sandbox' \
    || fail "codex exec help does not expose external sandbox mode"
codex exec --help | grep -q -- 'instructions are read from stdin' \
    || fail "codex exec help does not document stdin"

jq --null-input \
    --arg codex "$(codex --version)" \
    --arg executable "$(readlink /usr/local/bin/codex)" \
    '{schema_version: 1, codex: $codex, executable: $executable}' \
    >/artifacts/codex-smoke.json

printf 'codex smoke passed\n'
