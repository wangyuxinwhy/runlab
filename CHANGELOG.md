# Changelog

All notable public changes to RunLab are documented here.

## [0.1.0] - 2026-08-30

Initial public release.

### Added

- A pure OCI-based Run Protocol with explicit input validation, bounded standard streams, Secrets, timeout, cancellation, networking, multi-Program coordination, and optional Final Environment capture.
- A blocking Linux `NativeEngine` backed by `runc`, OverlayFS snapshots, process supervision, isolated and outbound-only networking, and content-addressed Final Images.
- Persistent caller-owned Run identities, idempotent acceptance, detached coordination, explicit cancellation and reconciliation, and immutable versioned Run Records.
- A local OCI content store and mutable Image Catalog with import, export, inspection, filesystem reading, and checked storage pruning.
- A bounded public SQL query plane for Runs, typed Observations, Observation retractions, and Run deletion tombstones.
- Ephemeral stderr NDJSON Live Events for execution progress and Program streams.
- An immutable Observation Type Registry shared by built-in and external Methods, with append-only correction and retraction history.
- Checked terminal-Run deletion with frozen asset fingerprints, stale-plan detection, retryable operation identities, and permanent Run identity tombstones.
- A fixed macOS Managed Linux VM that installs and verifies the same-version Linux RunLab binary and `runc` without introducing a second Engine.
- Version-matched bundled Markdown, public RSPress documentation, `llms.txt`, `llms-full.txt`, and per-page Markdown for Agent discovery.

### Compatibility

- RunLab database schema 6 is the first public storage contract. Earlier private development States are migrated where the implementation has direct evidence; newer schemas are rejected.
- The public CLI, JSON envelopes, Live Event shapes, Observation documents, Run deletion plans, and release manifest each carry their own explicit schema version.

[0.1.0]: https://github.com/wangyuxinwhy/runlab/releases/tag/v0.1.0
