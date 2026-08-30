---
title: Supported Platforms
description: Native Linux and macOS Managed VM release profiles.
---

# Supported Platforms

RunLab supports:

- Linux x86-64 and Linux ARM64 through `NativeEngine` and `runc`.
- Intel and Apple Silicon macOS through a fixed Lima/VZ Linux VM containing the same-version Linux RunLab binary.

Windows is not supported. macOS is not a separate execution backend: the host CLI transports exact inputs to the managed Linux data plane and verifies returned files and structured results.

The macOS profile supports declarative read-only VirtioFS shares. The VM configuration maps a macOS directory to `/mnt/runlab-shares/<name>`; OCI configuration performs the separate Guest-to-container bind. Share contents retain Host filesystem semantics and are external mutable state, so use an OCI Image when content identity or portable Linux semantics matter.

The release profile fixes the Rust toolchain, MSRV, `runc` version, Managed VM architecture, and expected Linux prerequisites. A platform claim is made only after the release artifact itself passes its native installation and execution gate.
