FROM node:24.3.0-bookworm-slim@sha256:8225b1806c6e37dced949224b5c0d8278a2fe593967288620e0af69b2cbc4539

RUN npm install --global @earendil-works/pi-coding-agent@0.84.2 \
    && test "$(pi --version)" = "0.84.2"

RUN mkdir -p /root/.pi/agent /opt/runlab-skills/repo-repair-workflow /experiments /verifier \
    && install -m 0600 /dev/null /root/.pi/agent/auth.json \
    && install -m 0600 /dev/null /verifier/verify.mjs

COPY skill/SKILL.md /opt/runlab-skills/repo-repair-workflow/SKILL.md
COPY tasks/query /experiments/query
COPY tasks/cache /experiments/cache
COPY tasks/merge /experiments/merge

RUN node --check /experiments/query/src/query.js \
    && node --check /experiments/cache/src/cache.js \
    && node --check /experiments/merge/src/merge.js

WORKDIR /experiments/query
ENTRYPOINT ["pi"]
