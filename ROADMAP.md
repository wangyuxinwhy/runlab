# Docker-free RunLab Roadmap

Status: implementation plan and completed checkpoints. Agent Wiki owns the settled target architecture; this file owns sequencing, gates, open engineering choices, and current delivery checkpoints.

## Target

RunLab natively owns OCI Image ingestion, inspection, filesystem rendering, file access, structural/filesystem diff, and Final Image construction. A native Linux execution path passes the accepted OCI Runtime `config.json` to an OCI runtime and captures filesystem changes through a RunLab-owned changeset encoder. Docker remains a compatibility adapter during migration and is not a long-term installation requirement.

```text
Local Catalog / OCI Distribution
                |
                v
         OCI Image Store
                |
       +--------+---------+
       |                  |
       v                  v
 Image read plane      Renderer
 inspect/file/diff     Layers -> rootfs
                          |
                          v
                 OCI Runtime Bundle
                 config.json + rootfs
                          |
                          v
                    OCI runtime
                          |
                          v
                 OverlayFS upperdir
                          |
                          v
                 Changeset encoder
                          |
                          v
                    Final OCI Image
```

On Linux the engine runs directly. On macOS all OCI filesystem semantics and execution state live inside a managed local Linux VM; the host process is a thin command and byte transport boundary.

## Fixed boundaries

- OCI Manifest digest remains Image identity. Catalog names and tags remain mutable references.
- Raw verified OCI bytes remain distinct from typed views. Derived indexes and rendered rootfs directories are rebuildable cache, never content authority.
- The OCI runtime only realizes `config.json`; it does not own image pull, rootfs rendering, network provisioning, stream policy, Run lifecycle, changeset capture, or Final Image construction.
- The native path starts as a concrete implementation. A generic backend or runtime trait is extracted only after two verified implementations expose real duplication.
- Unsupported filesystem metadata or runtime behavior fails closed before publishing a Final Manifest. A partial or ambiguous changeset is not a successful Final Image.
- Docker behavior is retained as comparison evidence during migration, not treated as the specification or the correctness oracle by itself.
- No new public command or field is added solely to expose an implementation experiment. Product vocabulary is added when the corresponding workflow and verification exist.
- Rust modules follow ownership boundaries, not workflow-themed `manager` layers. Data crosses boundaries as narrow typed facts; OCI raw bytes, filesystem bytes and process facts are not collapsed into `String` or generic JSON maps.
- Comments explain a non-obvious invariant, external specification constraint, unsafe-looking lifecycle choice or rejected alternative. Comments that merely restate names or control flow are removed; module and type structure must carry the ordinary explanation.
- Blocking subprocess and filesystem code remains the default. Async runtime, background daemon, generic backend framework and additional crates require a demonstrated workflow or measurable bottleneck.

## Work streams

### OCI read plane

Own verified ordered Layer traversal, whiteout and opaque semantics, hardlink resolution, regular-file reads, merged tar export, filesystem projections, and rebuildable path indexes. This stream must work with Docker absent.

### Changeset

Convert either a verified before/after filesystem comparison or a Linux OverlayFS upperdir into one OCI Layer. It owns byte paths, entry ordering, whiteout conversion, file metadata, hardlinks, xattrs, resource limits, DiffID, compression, and descriptor construction. Image assembly appends the resulting descriptor and DiffID to the parent Config and Manifest.

### Native Linux execution

Own private run directories, rootfs rendering, OverlayFS mount lifecycle, bundle creation, OCI runtime subprocess lifecycle, stdin/stdout/stderr, timeout/cancel, process and cgroup facts, cleanup, and orphan discovery. runc is the first reference runtime, invoked as a pinned subprocess. Youki v0.7.0 remains a negative conformance fixture until its observable lifecycle and cgroupfs resource semantics pass the same corpus.

### Docker-free image ingress

Verified OCI Layout/archive ingestion and public OCI Distribution pull are implemented. Local import uses read-only fd-relative Layout access or a bounded seekable tar index, traverses only Manifest graphs reachable from the root Index, preserves exact content bytes and rejects source/destination overlap before state initialization. Both local and registry ingress publish a Catalog reference only after descriptor, DiffID and Layer filesystem verification. The registry transport supports anonymous and Bearer-token reads; credentials, retry policy, push, referrers and signature verification remain at the host transport boundary and are future work.

### State integrity and retention

Own read-only verification of Run records, stored bytes, OCI graphs and every stored blob; explicit Catalog reference lifecycle; and reviewable OCI blob retention. Garbage collection is a plan/apply protocol under a state-wide maintenance barrier. It joins every root Index descriptor with accepted/terminal Run roots, refuses unresolved execution state, and can only shrink a stale plan's delete set when current reachability changes.

### macOS managed VM

First prove the complete Linux engine through an explicit Lima invocation. Then design a managed VM lifecycle and thin host CLI only after binary transport, signal propagation, state ownership, version negotiation, and recovery have executable evidence.

## Delivery phases

### Phase 0 — Freeze contracts and conformance corpus

Deliverables:

- Record the native Linux target in Agent Wiki while keeping current Docker-only facts separate.
- Freeze synthetic OCI Layer fixtures before renderer and changeset implementation. Include additions, modifications, explicit and opaque whiteouts, type replacement, nested directories, symlinks, hardlinks, long PAX paths, non-UTF-8 names, xattrs, capabilities, FIFO/device entries, corrupt descriptors, traversal attempts, and resource-limit cases.
- Define a semantic filesystem inventory used by comparisons: raw path bytes, type, mode, uid/gid, mtime, xattrs, link target, hardlink group, device numbers, and regular-file digest.
- Freeze the private byte-safe changeset vocabulary before writing either differ. Paths remain raw Linux bytes; display escaping is never identity.
- Preserve the current real Docker E2E as a baseline. Failures in either arm remain evidence and are not removed from scope.

Exit gate: the fixture corpus and comparison definition exist before implementation output is inspected.

### Phase 1 — Docker-free Image reads

Status: functional surface complete for the current in-memory renderer, including public filesystem diff and merged tar export. Persistent path indexing remains an optimization, not a correctness dependency.

Deliverables:

- Move Docker archive/cache mechanics out of the Image domain boundary.
- Implement verified ordered Layer input from the RunLab OCI Store.
- Replace `image file get` with a Docker-free path.
- Add an internal merged-tar renderer and a rebuildable path-index prototype; decide through the frozen corpus whether to adopt, patch, or only borrow from `ocirender`.
- Add structural Image comparison internally: Manifest ancestry, Config differences, and added Layer descriptors. Add public filesystem diff only after its output schema is demonstrated against real workflows.

Exit gate:

- `image inspect` and `image file get` pass with `docker` absent from `PATH` and no daemon available.
- Whiteout, opaque, hardlink, non-UTF-8, traversal, corruption, and bounded-memory cases pass.
- Renderer input is the exact descriptor selected by RunLab; no dependency chooses the first index entry or skips digest/size/DiffID validation.

### Phase 2 — Changeset engine independent of execution

Deliverables:

- Implement a before/after semantic tree comparator as the portable reference algorithm.
- Implement one deterministic Layer writer shared by every change source. It writes numeric ownership, fixed archive/compression metadata, unique raw-byte-sorted paths, OCI whiteouts, verified xattrs and hardlinks while streaming the DiffID and compressed digest.
- Keep the change model independent of its source so a later OverlayFS upperdir decoder can feed the same Layer writer.
- Apply every produced Layer back onto its parent and compare the resulting semantic inventory with the intended final tree.

Exit gate:

- Same accepted source snapshot produces identical Layer bytes.
- Every supported metadata class round-trips through Layer apply.
- Unknown overlay metadata, unsafe paths, unsupported entry types, concurrent mutation, or resource-limit violations fail before blob/Manifest publication.
- At least one independent OCI implementation applies the generated Final Image to the same semantic inventory; descriptor size/digest, DiffID, Layer count and history all verify.

### Phase 3 — Runtime boundary and OCI runtime feasibility

Deliverables:

- Separate OCI Runtime structural validation from the Docker capability profile without changing current Docker behavior.
- Reshape backend facts into common facts plus implementation-specific realized details; native code must never fabricate Docker context or Engine fields.
- Pin one runc version and probe it as an independent foreground subprocess with a hand-built bundle. Keep the pinned Youki probe as a negative comparison arm rather than a production fallback.
- Verify binary stdin, separate stdout/stderr, exit 0/non-zero, self-signal, fast exit, timeout, cancel, PID/cgroup observation, OOM evidence, process-tree cleanup and independent deadlines for state/kill/delete helpers.
- Record version-specific behavior such as whether foreground `run` retains runtime state after the init process exits. Do not infer signal or OOM provenance from an exit number.

Exit gate: all observations come from real subprocesses. If bounded stop/wait/cleanup, exact streams or required process facts cannot be implemented with the pinned version, stop integration and change the runtime version or boundary before adding native code. The unchanged Docker E2E still passes.

### Phase 4 — Rootful native Linux execution without Final Image

Deliverables:

- Render Initial Image into a private Linux filesystem.
- Mount `lowerdir + upperdir + workdir -> bundle/rootfs` with an explicitly recorded rootful OverlayFS profile. Begin with `metacopy=off`, `redirect_dir=nofollow`, `index=on` and `nfs_export=off`; reject an effective mount profile that differs.
- Put the accepted `config.json` into the bundle without translating it into Docker flags.
- Execute with a pinned runc binary through `create/start/kill/delete` or an equivalently observable lifecycle.
- Preserve current Run acceptance, exact streams, limits, cancellation, cleanup and terminal transaction semantics. During this phase the Final Image slot is explicitly unavailable so execution and capture are not debugged at once.
- Read exit and OOM facts from the runtime/process/cgroup boundary rather than inferring them from client exit alone.

Exit gate: a real Linux E2E runs with Docker absent and covers exact stdin, stdout/stderr, target exit 0 and non-zero, fast exit, timeout, capture limit, SIGINT/SIGTERM, setup failure, OOM evidence and cleanup. Every accepted failure terminalizes, and no tested path leaks a process, cgroup, runtime state or mount.

### Phase 5 — Native Final Image, fidelity and recovery

Deliverables:

- Stop the target process tree, unmount runtime-injected mounts, then compare the immutable lower rootfs with the stopped merged filesystem through the Phase 2 reference differ.
- Publish exactly one child Layer, including a deterministic empty Layer. Freshly rendering the Final Image must reproduce the captured merged filesystem inventory.
- Exercise OCI Runtime fields by feature family and reject unsupported host/runtime combinations during preflight.
- Implement bounded runtime subprocess waits and orphan discovery/reconciliation.
- Add crash points around mount, runtime create/start, capture, OCI publish, cleanup, and terminal transaction.
- Verify that OCI runtime features are distinguished from actual host capability and RunLab safety policy.
- Define network provisioning separately; an OCI runtime alone does not implement outbound-only `egress`.
- Keep a compact durable recovery journal until the terminal transaction succeeds. It identifies the Run, workspace, runtime ID, mount/cgroup facts, capture phase and any published Final descriptor.

Exit gate: no tested crash point silently loses an accepted Run, publishes an ambiguous Final Image, or leaves an unreported mount/runtime resource. Initial Layers are an exact prefix of Final Layers, all digests independently verify, and `render(Final Image)` equals the captured merged filesystem.

### Phase 6 — OverlayFS upperdir fast path

Deliverables:

- Implement a rootful upperdir decoder with explicit support for char-device and xattr whiteouts, opaque `y`/`x`, and filtering of known internal xattrs.
- Fail closed on `metacopy`, redirect data, unknown OverlayFS metadata, real `.wh.*` names, unsupported entries or an unclosed hardlink group.
- Run every fixture through both sources: `lower/merged` walking differ and upperdir decoder. Compare freshly rendered filesystem inventories, not Layer bytes, because two valid changesets may encode the same result differently.
- Retain the walking implementation as a permanent differential oracle even after the upperdir path becomes the production optimization.

Exit gate: both sources reconstruct the same intended filesystem for the complete supported corpus, with no unexplained difference against OCI specification and independent apply tools.

### Phase 7 — Rootless Linux and remaining ingress

Status: verified ingress, pre-acceptance Local Catalog resolution, explicit Catalog set/remove, state retention/GC, and the first restricted rootless execution profile are complete. Registry credentials/retry and Distribution push remain open.

Deliverables:

- Implement the selected first rootless profile: rootless runc with a single-ID user namespace, a directly writable materialized rootfs and the existing walking differ. The feasibility audit rejected rootless OverlayFS as the v1 correctness boundary because the required effective `index=on` profile and hardlink fidelity were not reliable in the ordinary Ubuntu VM.
- Fail closed outside the demonstrated rootless subset: uid/gid 0 inside the container, no devices, privileged xattrs or file capabilities, one participant, `network=none`, no resources/cgroup/OOM claims, no egress and no Managed Service. Expand the subset only with unchanged filesystem and runtime conformance fixtures.
- Preserve the implemented verified OCI Layout/archive ingestion and OCI Distribution pull as the public Docker-free source.
- Add registry credentials and retry policy without exposing secret values or weakening exact-byte verification; add push only after the same exact-byte and credential-boundary gate covers writes.
- Resolve local Catalog references to exact Manifest descriptors before Run acceptance.

Exit gate: an unprivileged Linux user can install or ingest a digest-pinned Image and complete the real native Run E2E without Docker or host-root mutation.

Rootless exit evidence: an ordinary uid 501 user on Ubuntu with runc 1.5.1 completed three real E2Es for execution/Final Image, pre-acceptance rejection outside the fixed subset, and supervisor-loss reconciliation. The implementation derives a single-ID OCI Runtime Config, records its digest/size and AppArmor invocation as backend realization facts, uses a directly writable materialized rootfs, and converts physical host uid/gid back to logical container 0/0 during capture. It does not use rootless OverlayFS or imply support for egress, Managed Service, resources, devices, privileged xattrs or host-file mounts.

### Phase 8 — Default switch and Docker demotion

Status: native is the CLI default, Docker commands are under the explicit `docker` namespace, the restricted rootless gate passes, and current native plus explicit Docker E2Es pass. A broader ordinary Linux distribution/kernel comparison corpus is still open, so this phase is not claimed as universally supported.

Deliverables:

- Run the frozen comparison corpus through both native and Docker paths with unchanged tasks and verifier definitions.
- Report semantic differences separately from Manifest byte differences and backend-specific facts.
- Make native Linux the default only after its platform, process, stream, filesystem, recovery, and security gates pass.
- Move Docker-specific import, cache, checkout and capture code behind an explicitly compatibility-scoped module and documentation section.

Exit gate: fresh Linux installation documentation and smoke tests contain no Docker prerequisite. Docker removal or absence cannot break OCI read, Catalog, schema, Run Record, or native Run commands.

### Phase 9 — macOS managed Linux VM

Status: the production-shaped transport and its main clean-host execution paths are implemented and verified on 2026-08-22. The host CLI pins limactl 2.2.0, creates from one embedded Ubuntu 24.04 release URL/digest without mutable fallback, validates a same-architecture plain Lima VZ instance with no mounts, digest-stages a same-version Linux binary and exact runc 1.5.1 identity, uses explicit rootful guest state namespaces, and runs recoverable systemd operations with get/attach/cancel/discard control. Install provisions exact `conntrack=1:1.4.8-1ubuntu1`, atomically persists and reads back OverlayFS module loading and `net.ipv4.ip_forward=1`, and fails unless canonical tools, cgroup v2, OverlayFS, systemd and current/persistent forwarding facts form a ready reference profile. `vm status` exposes the same typed facts, and `vm exec` rechecks them. A fresh arm64 VM completed clean create/install, full stop/start, OCI import, Runtime Config and exact-byte file transport, Primary-only native execution, cancellation, sealed read-only host input, one Managed Service, IPv4 egress, independent Final Images and Final Image re-execution. The complete transport-loss, disk-full, engine-upgrade, automatic release artifact and long-lived RunLab-owned VM image gates remain open.

Deliverables:

- First run the unmodified Linux RunLab binary inside a digest-pinned, same-architecture Lima VZ VM with `plain: true`, no host mounts and state on the VM disk. This is a semantic vertical slice, not yet the production transport.
- Verify non-PTY binary transport, exact stdin/stdout/stderr, exit status, SIGINT/SIGTERM, large files, interrupted host connection, VM restart, and state recovery.
- Define a versioned host/guest handshake and explicit digest-verified Image/file staging operations. Do not use a shared macOS directory as OCI Store, rootfs, upperdir or SQLite state.
- Resolve `--state` semantics before implementing the thin CLI. A host path cannot silently masquerade as the guest state directory; macOS v1 must either expose one managed default state or introduce an explicit engine-local state namespace with export/import operations.
- Use an explicit cancellation control path after acceptance. Killing an SSH connection is not Run cancellation and must not decide whether a Run continues.
- Only after the transport contract is proven, add managed VM lifecycle and thin host CLI behavior.

Exit gate: a clean macOS host can initialize and run Hello RunLab without Docker Desktop, while the user sees the same Run ID, descriptors, JSON shapes, byte outputs, and cancellation semantics as Linux. VM restart, transport loss, disk-full and engine upgrade tests cannot produce false success or orphan an unidentifiable accepted Run.

## Dependency decisions

- `ocirender`: evaluate the pinned low-level synchronous merge algorithm against the Phase 0 corpus. Do not use its layout resolver as authority or its current directory output as an execution rootfs. Adoption requires byte-path correctness, metadata/special-node fidelity, and bounded hardlink promotion.
- `oci-spec`: may be used as an OCI-module typed view, but does not replace RunLab's raw-byte identity, duplicate-key rejection, restricted protocol types, or unknown-field preservation.
- `oci-client`: not adopted for the current bounded pull workflow. `distribution.rs` uses blocking `reqwest` behind a narrow module boundary; reconsider only when credentials, push or referrers demonstrate enough protocol breadth to justify the dependency.
- `image-rs`: reference security and unpack tests only; do not import its CoCo/Kata pull, encryption, signature, and snapshot framework.
- runc: pin and invoke as a subprocess. Do not embed containerd or runc internal packages into RunLab core.
- Youki: retain v0.7.0 as a pinned negative conformance fixture. Reconsider it only after a released version passes the unchanged cgroupfs zero-swap, retained lifecycle and unambiguous exit/signal gates.

## First implementation checkpoint

Status: completed on 2026-08-21. The implemented read path is an in-memory, rebuildable filesystem view; persistent path indexing and full rootfs materialization remain later work.

The first code checkpoint is intentionally smaller than the native runtime:

1. Add the frozen OCI Layer conformance fixtures and semantic filesystem inventory.
2. Introduce an `image/render` boundary that consumes RunLab-verified Layer descriptors.
3. Make `image file get` independent of Docker.
4. Prototype structural Image diff without publishing a stable public schema.
5. Run the complete existing Rust/Docker verification unchanged to prove that separating the Image read plane did not regress the compatibility backend.

Evidence:

- a separate-process fixture with a failing fake `docker` in `PATH` reads exact bytes from plain, gzip and zstd Layers;
- whiteout, opaque, hardlink, non-UTF-8 identity, traversal, duplicate path, descriptor, DiffID, truncated compression, resource boundary and no-clobber cases pass;
- the unchanged real Docker E2E passes and reads the Docker-captured Final Image through the new OCI path.

Only after this checkpoint passes does work begin on the changeset writer. This gives immediate product value, exercises the hardest read-side semantics, and avoids coupling the first renderer decision to runtime or VM choices.

## Second implementation checkpoint

Status: completed on 2026-08-22 for the rootful Linux vertical slice described below. Rootless Linux, upperdir optimization, ordinary non-nested Linux conformance and macOS VM remain later phases rather than hidden checkpoint work.

Completed:

1. Extracted one deterministic Final Image assembler with explicit capture time and an exactly-one-Layer input.
2. Preserved Initial Config and Manifest extension fields while patching only the Final Config, Config descriptor and appended Layer.
3. Added raw-byte `FsPath`, semantic Inventory and a before/after comparator.
4. Added a deterministic fixed-gzip Layer encoder with private content spooling for regular files, directories, whiteouts and empty changesets.
5. Proved `compare → encode → Final assembly → Docker-free file get` with a repeated-input deterministic test.
6. Narrowed Docker capture to read-only archive validation plus ingestion of only the unique delta Layer.
7. Changed ordinary modifications to direct OCI entries; only real deletions emit whiteouts, and directory replacement suppresses impossible descendant whiteouts.
8. Added Inventory invariant validation and deterministic hardlink anchor promotion.
9. Added structural Layer encoding for symlink, hardlink, FIFO/device, signed/subsecond PAX mtime and binary xattrs through a shared length-aware PAX codec.
10. Added fd-relative Linux tree capture with capture-wide entries/path/xattr/content/depth budgets, hardlink-group consistency, and two agreeing full capture passes.
11. Separated backend-neutral Runtime validation from the Docker profile and added an independent private OCI bundle boundary that only consumes validated `RuntimeConfig`, without connecting it to Runner.
12. Reshaped Backend facts into common fields plus tagged Docker details, leaving native details undefined until the runtime probe produces evidence.
13. Removed implicit digest-derived Catalog reference creation from import and Final publication. Run assets directly retain Manifest descriptors; mutable Catalog names are explicit discovery metadata and use one locked atomic mutation path.
14. Added real LinuxKit/runc 1.3.6 and pinned Youki v0.7.0 lifecycle probes for exact bytes, exits, signals, cancellation, deadlines and cleanup. runc's UUID-scoped root-level OOM case realizes `memory.max=201326592` and `memory.swap.max=0`, proves `oom_kill` delta 1, retains stopped state with `--keep`, and cleans it through explicit delete. Youki remains a negative fixture because cgroupfs leaves swap unlimited, foreground status is ambiguous and `--keep` is not implemented. runc is the selected reference runtime mechanics boundary; 1.3.6 is a tested fixture rather than a long-term support pin.
15. Applied a RunLab-produced three-Layer Final Image with rootful `umoci 0.4.7`, recaptured it through the Linux fd-relative tree capture, and matched the intended full semantic Inventory for root/directory metadata, raw names, additions, modifications, explicit and opaque removals, hardlinks, binary `user.*` xattrs, FIFO and character device 1:3.
16. Added a Linux-only crate-private rootfs materializer that applies verified ordered Layers into an owned private directory with fd-relative no-follow operations. Regular content uses one bounded linear pass per Layer; hardlinks use dependency-chain resolution; final root/directory metadata is replayed across Layers. Linux fixtures cover semantic Inventory round-trip, opaque whiteout, forward hardlinks/cycles, recursive cleanup budgets and rootful character device 1:3.
17. Added a production-shaped crate-private `RuncRunner` over the validated bundle boundary. It uses pipe-drained byte streams, monotonic timeout/cancellation, explicit stopped-state observation and bounded helpers; post-exit observation/capture/cleanup failures preserve completed facts, and cleanup failure returns a runtime-root/container-id recovery handle.
18. Hardened the local OCI data plane with same-lock first initialization, staging outside the digest namespace, same-fd descriptor verification/read, Manifest body mediaType validation, concurrent Catalog mutation tests and pre-mutation SQLite storage-version rejection.
19. Connected materializer, fixed-profile OverlayFS, validated bundle and `RuncRunner` to public `run start --backend native`; native Final capture uses the common before/after changeset and exactly-one-Layer assembler without Docker.
20. Added a durable per-Run recovery attempt with private permissions, lock, monotonic atomic journal, stream sidecars and deterministic runtime/workspace ownership. Explicit `run reconcile` implements Wiki-defined orphan reconciliation and `supervisor_lost` terminalization without process restart or fabricated process facts.
21. Passed a real privileged Linux/runc CLI E2E covering exact bytes, exit, setup failure, timeout, capture limit, cancellation, Final Layer prefix, supervisor SIGKILL, read-only `run get`, pure reconcile dry-run, idempotent cleanup and absence of runtime/mount/scratch resources.
22. Added blocking OCI Distribution pull with explicit registry references, Bearer authentication, exact `linux/amd64|linux/arm64` platform selection, descriptor/media-type/DiffID verification and Catalog update only after the complete graph verifies.
23. Added an optional exactly-one required Managed Service participant with its own Image, Runtime Config, process/stream facts and Final Image. RunLab owns one private shared network namespace, readiness precedes Primary start, and both participants terminalize in one SQLite transaction; the namespace is loopback-only for `network=none` and gains the same outbound-only IPv4 realization for `network=egress`.
24. Added a native-only standard OCI read-only regular-file bind mount profile for ephemeral sensitive capability injection. Source and destination identities are pinned by fd and revalidated; source content is never read, hashed or persisted by RunLab.
25. Hardened runc and recovery after adversarial review: inherited descendant pipes have bounded drain, completed process facts survive observation/cleanup failure, durable facts are immutable, reconcile retries are idempotent, and start-pending recovery distinguishes absent runtime state from already-completed cleanup.
26. Passed a real PostgreSQL 17 Managed Service experiment: capture `DB₀`, mutate through Primary over shared loopback, capture distinct `DB₁`, restart from `DB₁`, and verify exact query output.

Remaining later-phase work:

1. Add a real concurrent-mutation fixture around the Linux fd-relative capture; two agreeing walks reduce but do not eliminate snapshot races.
2. Verify rootful `security.*`/`trusted.*` xattrs and stale directory-xattr removal under the native apply path; global PAX remains fail closed until a demonstrated image requires it. Negative fractional PAX mtime stays as a recorded libarchive interoperability difference even though the umoci oracle restores it correctly.
3. Add a RunLab-owned dedicated cgroup with an execution-start baseline before exposing OOM facts; the current native record truthfully leaves `oom_killed` unknown.
4. Expand crash injection beyond the running-process point to pre-acceptance, mount, Final publication and terminal transaction boundaries, and add host/runtime orphan discovery beyond durable recovery entries.
5. Broaden the ordinary Linux support matrix for the current exact runc 1.5.1 identity. The historical 1.3.6 nested LinuxKit fixture remains evidence about that checkpoint, not an installation recommendation.

## Third implementation checkpoint

Status: completed on 2026-08-22 for local discovery, Image analysis and Run history. It does not complete rootless Linux, macOS VM or the full Phase 8 comparison gate.

Completed:

1. Added a public Local Image Catalog with stable bounded `list`, verified `show`, normalized implicit `:latest` and local-only resolution.
2. Allowed Catalog references wherever the public read or execution surface previously required an exact Manifest digest, while preserving the requested reference and acceptance-time resolved descriptor in Primary and Managed Service Run inputs.
3. Added Docker-free `image diff` with structural and resolved-filesystem facts, byte-safe path identity and bounded cursor pagination.
4. Added Docker-free `image export` that emits one deterministic merged plain tar without materializing through Docker or overwriting an existing output.
5. Added bounded `run list` and fact-only `run diff`; neither path reads raw stream BLOBs.
6. Made native execution the default and moved Docker-specific Image import/materialize/checkout commands under `docker image`; Docker execution remains explicit through `--backend docker`.
7. Re-ran the current separate-process CLI/Image corpus, rootful Linux/runc default-backend E2E and explicit Docker compatibility E2E after the command migration.

Observed defects corrected during this checkpoint:

- The first merged export incorrectly reused the strict captured-tree `Inventory`, rejecting valid OCI layers with implicit parent directories. Export now builds the resolved changeset directly and retains OCI layer semantics.
- The first rootful verification copied a dynamically linked LinuxKit `ip` binary into an isolated PATH without its shared libraries. The product path was unchanged; the verifier now supplies a standard `iproute2` installation.
- The Docker compatibility E2E still called removed `image import/checkout` aliases and fed the native authoring helper's standard mounts into a Docker profile that explicitly rejects OCI mounts. The test now uses the public `docker image` namespace and explicitly authors a Docker-compatible no-mount config.

## Fourth implementation checkpoint

Status: completed on 2026-08-22 for the current rootful IPv4 egress profile, including normal completion, supervisor loss, deferred cleanup and explicit reconciliation.

Completed:

1. Added an acceptance-ordered durable `RunNetworkPlan` and recovery phases that distinguish plan reservation, host mutation, active use and cleanup.
2. Added a Run-owned network holder with exact PID start-time and namespace inode identity. Shutdown is an attempt-owned durable tombstone; reconciliation neither scans processes nor signals an unverified PID.
3. Added deterministic `/30` allocation inside `10.240.0.0/16`, host-network-scoped allocation serialization, one all-table IPv4 route snapshot with parent/exact/child overlap checks, stale conntrack checks and one total allocation deadline.
4. Added veth setup, guest default route and one atomic nftables ruleset for IPv4 source validation, host INPUT/OUTPUT isolation, cross-pool blocking, stateful return traffic and masquerade.
5. Added exact ownership checks before deletion. Veth ownership requires name, MAC, type and alias; nft ownership requires JSON family/name plus one exact owner comment from the text listing, including nftables 1.0.6 where JSON omits comments.
6. Added conntrack cleanup by guest original source between nft and veth removal, including idempotent zero-entry handling and a post-delete recheck. Allocation and cleanup share one host-network lock; a delayed cleanup with no owned veth rechecks the full route snapshot and skips conntrack deletion if the subnet has been reused.
7. Published `backend.run_network` terminal facts with loopback-only or IPv4 NAT realization details instead of exposing the private recovery journal shape.
8. Disabled IPv6 before either veth endpoint becomes active and verified both host sysctl readback and the absence of guest IPv6 addresses, closing the link-local path around the IPv4 policy.
9. Passed a fresh rootful Linux/runc packet E2E proving outbound FORWARD/NAT, host isolation, cross-pool isolation, terminal facts and absence of nft/veth/holder/conntrack residue.
10. Killed the supervisor after durable network activation and proved that explicit reconciliation terminalizes without restarting execution and removes only the recorded resources.
11. Injected a foreign nft ownership marker during normal cleanup and proved that the Run still becomes terminal with a `resource_cleanup` fact, the recovery attempt remains, and a later reconciliation removes it after the conflict is resolved.
12. Made allocation rollback consume the already-held host lock token instead of reacquiring the non-reentrant lock. Binding and checkpoint failure paths release the reservation before ordinary cleanup, and a Linux regression test exercises cleanup while the allocation lock remains held.

Observed defects corrected during this checkpoint:

- nftables 1.0.6 omits a table comment from `--json list table`; requiring the JSON field made safe cleanup fail. The implementation now retains exact JSON identity checks and independently verifies the text comment before deletion.
- conntrack-tools 1.4.7 accepts `-L/-D` as commands but rejects the guessed `--list` spelling. The real executable failure was retained, and both production and verification now use the observed command surface.
- The first allocation rollback reacquired its own host-network lock and timed out instead of preserving the setup error. The lock reservation is now an explicit token passed through setup and rollback rather than a hidden process-global precondition.

Remaining work belongs to the broader native execution gates: a wider ordinary Linux matrix, rootless execution, additional crash points and host/runtime orphan discovery beyond durable recovery entries. It is not part of this egress checkpoint.

## Fifth implementation checkpoint

Status: completed on 2026-08-22 for Docker-free local OCI Layout/archive ingress. Catalog lifecycle was left to the sixth checkpoint; registry write/credentials and execution portability remain separate gates.

Completed:

1. Added `image import SOURCE --name REFERENCE` for a read-only OCI Layout directory or plain OCI tar archive, without a Docker executable or daemon.
2. Added bounded nested Index traversal, exact reachable `--manifest` selection, root `--source-reference` selection and Config-backed platform resolution.
3. Preserved exact Manifest, Config and Layer bytes while verifying descriptor digest/size/mediaType, ordered DiffIDs and the complete Layer filesystem view before Catalog publication.
4. Added fd-relative `NOFOLLOW` Layout reads and a seekable archive member index. Archive preflight rejects unsafe or duplicate normalized paths, non-regular members, sparse/global PAX, PAX size overrides, malformed checksum/truncation, nonzero trailing data and aggregate resource-limit violations.
5. Rejected source/destination overlap before RunLab initializes or changes state, including the case where an externally authored source already occupies `<state>/oci`.
6. Kept Catalog update last and under the existing locked atomic index mutation. Corrupt imports leave an existing reference and its index bytes unchanged; two importer processes preserve both names, while the deterministic concurrent Catalog unit test exercises actual lock contention.
7. Added the public `image-import-result` schema and independent-process tests for read-only Layout/archive success, exact byte identity, downstream file reads, selection ambiguity, nested Index, platform mismatch, unrooted Manifest, corruption, archive attacks, overlap, concurrent names and a Docker sentinel.

Observed defects corrected during this checkpoint:

- Exact `--manifest` initially hydrated every unrelated platform-less candidate, so one irrelevant corrupt Config could block the selected graph. Exact selection now hydrates only the selected candidate.
- Destination initialization initially happened before overlap rejection and could chmod or add lock/staging entries to a source occupying `<state>/oci`. Boundary validation now runs before any state mutation, and the regression compares the complete source tree, bytes and modes.
- Archive preflight and tar parsing could disagree when local PAX overrode the next entry size. Transport PAX size overrides are now rejected, and all PAX/GNU path extension payloads share an aggregate budget.
- A FIFO masquerading as a Layout member could block before its file type was checked. Members now open with `NONBLOCK|NOFOLLOW`; the dedicated regression is Linux-only.
- Unsupported descriptor platforms were initially collapsed into a missing platform and could be reinterpreted from Config. The raw declared claim is now retained and must agree with the selected Config.

Current clean-target evidence is unit 125 passed/2 ignored, CLI contract 14 passed, Image read integration 5 passed and OCI import integration 13 passed; `cargo clippy --all-targets --all-features -- -D warnings` passes. The Linux-only FIFO regression is compiled and runs at the ordinary-Linux gate; backend/probe tests remain opt-in and this checkpoint does not change their evidence scope.

## Sixth implementation checkpoint

Status: Catalog lifecycle, Run/state verification and plan/apply OCI blob GC are implemented and accepted on 2026-08-22. Fresh macOS/Linux all-target quality gates and real native/Docker regressions pass after the combined changes.

Completed:

1. Added `image catalog set` for explicit create/tag move and mutable description set/clear, and idempotent `image catalog remove` that never deletes OCI content.
2. Added read-only `RunDatabase::open_existing` and one deferred retention snapshot that cross-checks lifecycle, accepted/terminal JSON, SQL identity projections and every stored byte digest/size without initializing schema or creating WAL sidecars.
3. Added `run verify` for one retained Run and `state verify` for the root Index, Catalog, all Run records, rooted Image graphs, every stored blob, staging entries, recovery entries and reachable/orphan accounting.
4. Added a state-wide shared/exclusive operation lock. Ordinary resolve/accept, OCI publish, Catalog mutation and Final publication hold a shared lease; GC plan/apply hold the exclusive maintenance lease while the existing OCI index locks and SQLite transactions continue to serialize their narrower writes.
5. Added `state gc plan --output FILE` with no-clobber canonical JSON, typed roots, exact candidates, roots digest and plan digest. Roots include every root Index Manifest descriptor, even without a Catalog annotation, plus every participant Initial Image and each available terminal Final Image retained by a Run.
6. Added `state gc apply PLAN` with schema/digest/sort validation, a fresh complete state verification, reachability recheck, digest/size preverification before the first unlink, directory `fsync`, replay accounting and bounded failure details. A stale plan skips content that became reachable and never expands to content that became orphaned later.
7. Made both plan and apply fail closed while any Run is accepted or any native recovery entry remains, so GC cannot guess at transient execution assets.
8. Expanded the public schema registry from 22 to 30 typed result/document schemas for Catalog mutation, verification and GC at this checkpoint; later managed VM commands bring the current registry to 36.
9. Added independent-process regression coverage for Catalog move/idempotence/description clear/content retention, missing-state verification, orphan reporting, tampered and replayed plans, recovery refusal, stale-plan shrink-only behavior, and Docker absence. The real native E2E asserts accepted/recovery refusal and proves a terminal Run's Initial and Final Images survive a successful GC.

Remaining after this checkpoint:

- broader rootless and ordinary Linux distribution/kernel support beyond the fixed demonstrated profiles;
- macOS managed VM transport-loss, disk-full, engine-upgrade, automatic artifact and long-lived image gates;
- Distribution credentials/retry, push, referrers/signature verification and official Catalog provenance;
- broader crash injection and host/runtime orphan discovery beyond the current recovery directory;
- fresh all-target, Clippy, MSRV, package, installed CLI, native runc and Docker compatibility verification for the combined worktree.

## Plan discipline

- Record implementation facts in `IMPLEMENTATION.md`; do not present a phase as implemented before its gates pass.
- When a comparison fixture, inventory definition, or verifier changes, rerun every affected comparison arm. Results from a changed or concurrent evaluation path are void.
- Keep phase-specific notes here until stable. Promote settled architecture to Agent Wiki and remove obsolete implementation checkpoints instead of allowing this file to become a second protocol.
