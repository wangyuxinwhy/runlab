#!/bin/sh
# RunLab contract: read the whole Task instruction from stdin, work in
# /workspace, write deliverables under /artifacts, exit nonzero on failure.
set -eu

DEEPSEEK_API_KEY="$(cat /run/credentials/deepseek)"
export DEEPSEEK_API_KEY

# An Overlay supplies capability files by mounting them at a fixed path. The
# Base owns how the runtime consumes them, so an Overlay never has to name a
# pi command-line flag.
set --
if [ -d /opt/runlab/skills ]; then
  set -- "$@" --skill /opt/runlab/skills
fi

exec pi --print \
  --provider "${PI_PROVIDER:-deepseek}" \
  --model "${PI_MODEL:-deepseek-v4-flash}" \
  --session-dir /root/.pi/agent/sessions \
  "$@"
