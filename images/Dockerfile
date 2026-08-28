# syntax=docker/dockerfile:1.7

FROM ghcr.io/astral-sh/uv:0.12.7@sha256:95f2aa1fe59274951cfe9b0cbc7972e879ff1004bc8945d130a32eb0dbd85945 AS uv

FROM ubuntu:24.04@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517 AS base

SHELL ["/bin/bash", "-o", "pipefail", "-c"]

RUN --mount=type=bind,source=packages.system.txt,target=/tmp/packages.txt,readonly \
    --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    apt-get update \
    && DEBIAN_FRONTEND=noninteractive xargs --no-run-if-empty apt-get install --yes --no-install-recommends </tmp/packages.txt \
    && rm -f /var/log/apt/* /var/log/dpkg.log

RUN --mount=type=bind,source=packages.python.txt,target=/tmp/packages.txt,readonly \
    --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt/lists,sharing=locked \
    apt-get update \
    && DEBIAN_FRONTEND=noninteractive xargs --no-run-if-empty apt-get install --yes --no-install-recommends </tmp/packages.txt \
    && rm -f /var/log/apt/* /var/log/dpkg.log

ARG TARGETARCH
ARG NODE_VERSION=24.20.0
ARG NODE_ARM64_SHA256=5f4ddab610c1ab2016b3c227cebdbf6d9495161487e4739c7b90090595f465f7
ARG NODE_AMD64_SHA256=2f2c0da162318f0de47665410c7c8c2ed3d36c8f3105de4bbc61176c70a7cbf2

RUN case "$TARGETARCH" in \
        arm64) node_arch=arm64; node_sha256="$NODE_ARM64_SHA256" ;; \
        amd64) node_arch=x64; node_sha256="$NODE_AMD64_SHA256" ;; \
        *) echo "unsupported Node.js target architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && node_archive="node-v${NODE_VERSION}-linux-${node_arch}.tar.xz" \
    && curl --fail --silent --show-error --location \
        "https://nodejs.org/dist/v${NODE_VERSION}/${node_archive}" \
        --output "/tmp/${node_archive}" \
    && printf '%s  %s\n' "$node_sha256" "/tmp/${node_archive}" | sha256sum --check --strict \
    && tar --extract --xz --file "/tmp/${node_archive}" --directory /usr/local --strip-components=1 \
    && rm "/tmp/${node_archive}"

COPY --from=uv /uv /uvx /usr/local/bin/

RUN usermod --login agent --comment Agent --home /home/agent --move-home --shell /bin/bash ubuntu \
    && groupmod --new-name agent ubuntu \
    && chmod 0755 /home/agent \
    && install --directory --owner=agent --group=agent --mode=0755 \
        /home/agent/.cache \
        /home/agent/.config \
        /home/agent/.local/bin \
        /home/agent/.local/share \
        /home/agent/.local/state \
        /workspace \
        /artifacts \
    && install --directory --owner=root --group=root --mode=0755 /opt/agents \
    && ln --symbolic /usr/bin/fdfind /usr/local/bin/fd

ENV HOME=/home/agent \
    LANG=C.UTF-8 \
    LC_ALL=C.UTF-8 \
    TERM=xterm \
    PATH=/home/agent/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin \
    XDG_CACHE_HOME=/home/agent/.cache \
    XDG_CONFIG_HOME=/home/agent/.config \
    XDG_DATA_HOME=/home/agent/.local/share \
    XDG_STATE_HOME=/home/agent/.local/state \
    NPM_CONFIG_PREFIX=/home/agent/.local

USER 1000:1000
WORKDIR /workspace
CMD ["/bin/bash"]

FROM base AS pi

USER 0:0

ARG PI_VERSION=0.84.3

RUN --mount=type=cache,target=/home/agent/.npm,sharing=locked \
    --mount=type=cache,target=/tmp/node-compile-cache,sharing=locked \
    npm install --global --prefix /opt/agents/pi \
        --no-audit \
        --no-fund \
        "@earendil-works/pi-coding-agent@${PI_VERSION}" \
    && ln --symbolic /opt/agents/pi/bin/pi /usr/local/bin/pi \
    && install --directory --owner=agent --group=agent --mode=0700 \
        /home/agent/.pi \
        /home/agent/.pi/agent \
    && test "$(pi --version)" = "$PI_VERSION"

USER 1000:1000
ENTRYPOINT ["/opt/agents/pi/bin/pi"]
CMD ["--approve"]

FROM base AS claude

USER 0:0

ARG CLAUDE_VERSION=2.1.250

RUN --mount=type=cache,target=/home/agent/.npm,sharing=locked \
    --mount=type=cache,target=/tmp/node-compile-cache,sharing=locked \
    npm install --global --prefix /opt/agents/claude \
        --no-audit \
        --no-fund \
        "@anthropic-ai/claude-code@${CLAUDE_VERSION}" \
    && ln --symbolic /opt/agents/claude/bin/claude /usr/local/bin/claude \
    && install --directory --owner=agent --group=agent --mode=0700 \
        /home/agent/.claude \
    && test "$(claude --version)" = "$CLAUDE_VERSION (Claude Code)"

USER 1000:1000
ENTRYPOINT ["/opt/agents/claude/bin/claude"]
CMD ["--print", "--dangerously-skip-permissions"]

FROM base AS codex

USER 0:0

ARG CODEX_VERSION=0.150.1

RUN --mount=type=cache,target=/home/agent/.npm,sharing=locked \
    --mount=type=cache,target=/tmp/node-compile-cache,sharing=locked \
    --mount=type=tmpfs,target=/home/agent/.codex \
    npm install --global --prefix /opt/agents/codex \
        --no-audit \
        --no-fund \
        "@openai/codex@${CODEX_VERSION}" \
    && ln --symbolic /opt/agents/codex/bin/codex /usr/local/bin/codex \
    && test "$(codex --version)" = "codex-cli $CODEX_VERSION"

RUN install --directory --owner=agent --group=agent --mode=0700 \
    /home/agent/.codex

USER 1000:1000
ENTRYPOINT ["/opt/agents/codex/bin/codex"]
CMD ["exec", "--dangerously-bypass-approvals-and-sandbox", "--skip-git-repo-check", "-"]

FROM pi AS all

USER 0:0

COPY --from=claude /opt/agents/claude /opt/agents/claude
COPY --from=codex /opt/agents/codex /opt/agents/codex

RUN install --directory --owner=agent --group=agent --mode=0700 \
    /home/agent/.claude \
    /home/agent/.codex

ENV PATH=/opt/agents/pi/bin:/opt/agents/claude/bin:/opt/agents/codex/bin:/home/agent/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

USER 1000:1000
ENTRYPOINT []
CMD ["/bin/bash"]
