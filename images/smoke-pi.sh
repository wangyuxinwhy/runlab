#!/bin/bash
set -euo pipefail

fail() {
    printf 'pi smoke failed: %s\n' "$1" >&2
    exit 1
}

test "$(id -u)" = 1000 || fail "uid is not 1000"
test "$(id -g)" = 1000 || fail "gid is not 1000"
test "$HOME" = /home/agent || fail "HOME is not /home/agent"
test "$PWD" = /workspace || fail "working directory is not /workspace"

test "$(command -v pi)" = /usr/local/bin/pi || fail "pi is not on PATH"
test "$(readlink /usr/local/bin/pi)" = /opt/agents/pi/bin/pi \
    || fail "pi does not resolve to the isolated install"
test "$(pi --version)" = 0.84.3 || fail "unexpected pi version"
test ! -w /opt/agents/pi || fail "/opt/agents/pi is writable"
test -w /home/agent/.pi/agent || fail "pi state directory is not writable"

pi --help | grep -q -- '--provider' || fail "pi help does not expose --provider"
pi --help | grep -q -- '--model' || fail "pi help does not expose --model"

jq --null-input \
    --arg pi "$(pi --version)" \
    --arg executable "$(readlink /usr/local/bin/pi)" \
    '{schema_version: 1, pi: $pi, executable: $executable}' \
    >/artifacts/pi-smoke.json

printf 'pi smoke passed\n'
