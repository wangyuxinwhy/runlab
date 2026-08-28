#!/bin/bash
set -euo pipefail

fail() {
    printf 'claude smoke failed: %s\n' "$1" >&2
    exit 1
}

test "$(id -u)" = 1000 || fail "uid is not 1000"
test "$(id -g)" = 1000 || fail "gid is not 1000"
test "$PWD" = /workspace || fail "working directory is not /workspace"

test "$(command -v claude)" = /usr/local/bin/claude || fail "claude is not on PATH"
test "$(readlink /usr/local/bin/claude)" = /opt/agents/claude/bin/claude \
    || fail "claude does not resolve to the isolated install"
test "$(claude --version)" = "2.1.250 (Claude Code)" || fail "unexpected claude version"
test ! -w /opt/agents/claude || fail "/opt/agents/claude is writable"
test -w /home/agent/.claude || fail "claude state directory is not writable"

claude --help | grep -q -- '--print' || fail "claude help does not expose --print"
claude --help | grep -q -- '--dangerously-skip-permissions' \
    || fail "claude help does not expose permission bypass"

jq --null-input \
    --arg claude "$(claude --version)" \
    --arg executable "$(readlink /usr/local/bin/claude)" \
    '{schema_version: 1, claude: $claude, executable: $executable}' \
    >/artifacts/claude-smoke.json

printf 'claude smoke passed\n'
