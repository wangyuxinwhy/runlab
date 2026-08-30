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

Install the prebuilt release with the same installer, or build from crates.io:

```bash
curl --proto '=https' --tlsv1.2 -fsSL \
  https://github.com/wangyuxinwhy/runlab/releases/latest/download/install.sh \
  -o install-runlab.sh
sh install-runlab.sh

cargo install runlab --version 0.1.1 --locked
```

The GitHub Release installer includes the verified runc 1.5.1 binary as `runlab-runc`. RunLab prefers this private sibling executable, so a complete Release installation neither replaces nor depends on an unrelated system `runc`. Cargo installation requires Rust 1.95 or newer and uses `runc` from `PATH`; RunLab 0.1.x requires that runtime to report exact OCI Runtime Specification 1.3.0 support and expose `runc create --pidfd-socket`. The tested runtime is runc 1.5.1.

Native execution uses the rootful Linux reference profile. It requires the unified cgroup v2 hierarchy mounted at `/sys/fs/cgroup` and OverlayFS; cgroup v1 and hybrid layouts are unsupported. Egress mode additionally requires the `ip`, `iptables`, `ip6tables`, and `nsenter` executables plus `net.ipv4.ip_forward=1`. On Debian and Ubuntu the executables are normally provided by:

```bash
apt-get install iproute2 iptables util-linux
sysctl net.ipv4.ip_forward
```

The second command only inspects forwarding. RunLab and its installer do not install host packages, enable forwarding, or change firewall policy.

## Verify the installation

```bash
runlab --version
runlab --help
runlab docs get start-here
```

The command, bundled documentation, and public schema are version-matched. Do not substitute interface names remembered from another release.
