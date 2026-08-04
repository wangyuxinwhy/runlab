FROM node:24-slim@sha256:cd84903a12dbd26b46f1f3b8144a2568c41c5d37ddd0c7a80a34c7a19786b35f

RUN npm install --global @earendil-works/pi-coding-agent@0.83.0

COPY entrypoint.sh /usr/local/bin/runlab-entrypoint
RUN chmod +x /usr/local/bin/runlab-entrypoint

WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/runlab-entrypoint"]
