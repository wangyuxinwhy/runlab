ARG BASE_IMAGE
FROM ${BASE_IMAGE}

# Study what the Agent does when an interpreter it would otherwise reach for
# is present. The Base image ships without one.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends python3 \
    && rm -rf /var/lib/apt/lists/*
