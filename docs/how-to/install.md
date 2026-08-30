---
title: Install RunLab
description: Install RunLab on Linux or install the complete macOS Managed VM bundle.
---

# Install RunLab

RunLab executes OCI workloads on Linux. macOS uses a fixed local Linux VM and therefore needs a release bundle containing the macOS CLI, a matching Linux RunLab binary, and `runc`.

## macOS

Install Lima 2.2.0, then use the verified GitHub Release installer:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/wangyuxinwhy/runlab/releases/latest/download/install.sh \
  -o install-runlab.sh
sh install-runlab.sh
runlab vm create
runlab vm install
runlab vm status
```

The installer verifies the selected archive before publishing files. `vm install` then verifies and atomically installs the bundled Linux RunLab and `runc` inside the managed VM.

## Linux

Install the prebuilt release binary with the same installer, or build from crates.io:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/wangyuxinwhy/runlab/releases/latest/download/install.sh \
  -o install-runlab.sh
sh install-runlab.sh

cargo install runlab --version 0.1.0 --locked
```

The native execution path additionally requires the prerequisites reported by `runlab vm status` on macOS or the equivalent reference profile on Linux, including `runc`, cgroup v2, OverlayFS, and the networking tools used by egress mode.

## Verify the installation

```bash
runlab --version
runlab --help
runlab docs get start-here
```

The command, bundled documentation, and public schema are version-matched. Do not substitute interface names remembered from another release.
