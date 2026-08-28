#!/bin/bash
set -euo pipefail

fail() {
    printf 'base smoke failed: %s\n' "$1" >&2
    exit 1
}

test "$(id -u)" = 1000 || fail "uid is not 1000"
test "$(id -g)" = 1000 || fail "gid is not 1000"
test "$HOME" = /home/agent || fail "HOME is not /home/agent"
test "$PWD" = /workspace || fail "working directory is not /workspace"

for directory in /home/agent /workspace /artifacts; do
    test -w "$directory" || fail "$directory is not writable"
done
test ! -w /opt/agents || fail "/opt/agents is writable"

python_version="$(python --version 2>&1)"
python3_version="$(python3 --version 2>&1)"
[[ "$python_version" == Python\ 3.12.* ]] || fail "unexpected python version: $python_version"
[[ "$python3_version" == Python\ 3.12.* ]] || fail "unexpected python3 version: $python3_version"

uv_version="$(uv --version)"
node_version="$(node --version)"
npm_version="$(npm --version)"
npx_version="$(npx --version)"
[[ "$uv_version" == "uv 0.12.7 "* ]] || fail "unexpected uv version: $uv_version"
test "$node_version" = v24.20.0 || fail "unexpected Node.js version: $node_version"
test -n "$npm_version" || fail "npm did not report a version"
test -n "$npx_version" || fail "npx did not report a version"

for command in autoconf automake bash bzip2 cc cmake curl diff dig fd file find gawk git grep gzip ip jq less libtoolize lsof make nc ninja npx npm node patch ping pkg-config ps pstree rg rsync sed ssh sqlite3 strace tar tree unzip uv uvx wget xz zip zstd; do
    command -v "$command" >/dev/null || fail "command is missing: $command"
done
git lfs version >/dev/null || fail "git-lfs is unavailable"

while IFS= read -r package; do
    dpkg-query --show --showformat='${db:Status-Status}\n' "$package" 2>/dev/null | grep -qx installed \
        || fail "package is not installed: $package"
done </dev/stdin <<'PACKAGES'
autoconf
automake
bash
build-essential
bzip2
ca-certificates
cmake
coreutils
curl
diffutils
dnsutils
fd-find
file
findutils
gawk
git
git-lfs
grep
gzip
iproute2
iputils-ping
jq
less
libffi-dev
libssl-dev
libtool
lsof
netcat-openbsd
ninja-build
openssh-client
patch
pkg-config
procps
psmisc
python-is-python3
python3
python3-dev
python3-pip
python3-venv
ripgrep
rsync
sed
sqlite3
strace
tar
tree
unzip
wget
xz-utils
zip
zstd
PACKAGES

for path in \
    /home/agent/.claude \
    /home/agent/.codex \
    /home/agent/.pi \
    /workspace/.git \
    /workspace/task.md; do
    test ! -e "$path" || fail "unexpected preloaded state: $path"
done

test -z "$(find /home/agent/.cache -mindepth 1 -print -quit)" \
    || fail "base contains a populated user cache"

jq --null-input \
    --arg uid "$(id -u)" \
    --arg gid "$(id -g)" \
    --arg home "$HOME" \
    --arg cwd "$PWD" \
    --arg python "$python_version" \
    --arg uv "$uv_version" \
    --arg node "$node_version" \
    --arg npm "$npm_version" \
    '{schema_version: 1, uid: $uid, gid: $gid, home: $home, cwd: $cwd, versions: {python: $python, uv: $uv, node: $node, npm: $npm}}' \
    >/artifacts/base-smoke.json

printf 'base smoke passed\n'
