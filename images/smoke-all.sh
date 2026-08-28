#!/bin/bash
set -euo pipefail

fail() {
    printf 'all smoke failed: %s\n' "$1" >&2
    exit 1
}

test "$(id -u)" = 1000 || fail "uid is not 1000"
test "$(id -g)" = 1000 || fail "gid is not 1000"
test "$PWD" = /workspace || fail "working directory is not /workspace"

test "$(pi --version)" = 0.84.3 || fail "unexpected pi version"
test "$(claude --version)" = "2.1.250 (Claude Code)" || fail "unexpected claude version"
test "$(codex --version)" = "codex-cli 0.150.1" || fail "unexpected codex version"

for agent_directory in pi claude codex; do
    test ! -w "/opt/agents/$agent_directory" \
        || fail "/opt/agents/$agent_directory is writable"
done

for state_directory in /home/agent/.pi/agent /home/agent/.claude /home/agent/.codex; do
    test -w "$state_directory" || fail "$state_directory is not writable"
done

jq --null-input \
    --arg pi "$(pi --version)" \
    --arg claude "$(claude --version)" \
    --arg codex "$(codex --version)" \
    '{schema_version: 1, pi: $pi, claude: $claude, codex: $codex}' \
    >/artifacts/all-smoke.json

printf 'all smoke passed\n'
