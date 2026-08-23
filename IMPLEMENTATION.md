# Current Rust OCI Run Vertical Slice

本文件记录 `/Users/bytedance/workspace/temp/runlab-protocol` 当前未提交 worktree 的实现事实。Agent Wiki 拥有正式产品、协议、系统设计和软件架构；这里不建立第二份规范。

正式目标已经改为 Docker-free Image data plane、RunLab-owned changeset、native Linux + OCI runtime reference execution，以及 macOS managed Linux VM。公开 `run start` 默认选择 native backend，并已贯通 verified Image pull/materialization、rootless single-ID execution、OCI bundle、runc、dedicated cgroup、exact streams、Final OCI Image、一个 required Managed Service、outbound-only IPv4 egress、durable recovery journal 与显式 orphan reconciliation；Docker 只能通过 `--backend docker` 显式选择。macOS managed VM 已通过固定 upstream image 的 clean create/install/restart 和主要执行场景，但自动 release artifact、长期自有 VM image、transport-loss、disk-full 与 upgrade failure matrix 仍未达到完整 production gate。具体交付顺序与 gate 见 [ROADMAP.md](ROADMAP.md)，不能把已通过的纵切外推成所有主机组合都已证明。

## 实现选择

当前产品只有一个 Rust 实现和一个 `runlab` binary，没有 Python compatibility package、第二套 CLI 或 async runtime。

- Rust edition 2024，声明 MSRV 1.95，固定开发工具链 1.97.1；
- 单 binary crate，`unsafe_code = "forbid"`；
- blocking `std::process`、filesystem 和 worker thread，不使用 Tokio；
- `clap`、`serde`、`rusqlite`，不用 Docker SDK、ORM 或 container framework；
- Docker CLI 是公开 compatibility backend；rootful Linux 的 reference path 直接调用固定版本 runc，但仍不为两个实现提前抽象通用 `Backend` trait；
- content-addressed OCI object 始终保留和校验原始 bytes，不通过 deserialize/serialize 代替内容身份。

源码责任为：

```text
src/main.rs       single binary composition root
src/cli.rs        noun–verb CLI、JSON stdout、exit status
src/core.rs       Descriptor、Run、process、content-slot facts
src/integrity.rs  exact bytes、SHA-256、canonical JSON、private output
src/oci.rs        OCI Layout bytes、same-fd verification、atomic publish、index lock
src/catalog.rs    Local reference grammar、Catalog metadata 与 resolve/list/set/remove
src/ingress.rs    read-only OCI Layout/archive graph selection、验证与 exact-byte ingest
src/distribution.rs OCI Distribution pull、auth、platform selection
src/image_ingress.rs import/pull application workflow 与 Catalog publication
src/filesystem/   raw-byte path、semantic inventory、content spool 与 deterministic tar/PAX primitives
src/changeset/    before/after comparison、OCI whiteout semantics 与 staged Layer encoding
src/image.rs      OCI Image inspect/diff/export、staged Layer publication 与 Final assembly
src/filesystem/pax.rs bounded length-aware PAX codec 与 sparse ordinal index
src/render.rs     verified Layers、byte-safe filesystem view 与 file streaming
src/materialize.rs Linux-only verified Layers → private rootfs materialization
src/runtime.rs    Runtime config structural/typed check、canonical bytes、authoring
src/bundle.rs     private OCI Runtime bundle 与 rootfs/config.json ownership
src/docker/       显式 compatibility adapter、Docker process lifecycle 与 Image bridge
src/native_backend.rs native host/runtime/filesystem/network preflight 与 realization
src/native_network/ durable private namespace、IPv4 egress 与 host resource ownership
src/native_recovery/ durable native attempt journal、journal validation 与 private layout
src/native_reconcile.rs interrupted native Run reconciliation
src/read_only_file.rs native read-only regular-file mount identity guards
src/native_backend/runc.rs Linux-only runc identity、subprocess lifecycle、streams 与 raw observations
src/execution/    acceptance → execute → terminal orchestration；Managed Service workflow 独立成模块
src/reconciliation.rs reconciliation 公开结果类型
src/storage.rs    SQLite transactions、immutable Run records、exact bytes
src/state.rs      state directory 上的 ordinary/maintenance process lock；只读命令不创建 lock file
src/maintenance.rs Run/state verification、retention graph 与 plan/apply GC
src/managed_vm/ Lima lifecycle、versioned guest control、bounded subprocess、digest-verified file staging 与 recoverable operation
src/topology.rs   bounded one-Service declaration 与 TCP readiness input
```

## 当前工作变换

```text
(Primary Manifest, Primary Runtime config.json, Run Controls, optional Managed Service)
→ (one terminal Run Record, Primary Final Manifest, optional Service Final Manifest)
```

Runtime Config 在 acceptance 前规范化为 compact JSON 加换行，数据库保存这些 accepted bytes；它不假装保存用户输入文件的原始排版。stdin、stdout 和 stderr 以 bytes 处理，不假设 UTF-8。

## CLI 与 State

当前命令树：

```text
runlab [--state DIRECTORY]
├── image
│   ├── import
│   ├── pull
│   ├── catalog list
│   ├── catalog show
│   ├── catalog set
│   ├── catalog remove
│   ├── inspect
│   ├── diff
│   ├── export
│   └── file get
├── docker
│   └── image
│       ├── import
│       ├── materialize
│       ├── checkout create
│       └── checkout commit
├── runtime-config
│   ├── create
│   └── check
├── managed-service
│   └── check
├── vm
│   ├── create
│   ├── status
│   ├── start
│   ├── install
│   ├── exec
│   └── operation get|attach|cancel|discard
├── run
│   ├── start
│   ├── get
│   ├── verify
│   ├── list
│   ├── diff
│   ├── reconcile
│   ├── stdout get
│   └── stderr get
├── state
│   ├── verify
│   └── gc
│       ├── plan
│       └── apply
└── schema
    ├── list
    └── show
```

成功响应在 stdout 输出单行紧凑 JSON，diagnostic 写 stderr。成功 operation 返回 0，RunLab operation error 返回 1，Clap usage error 返回 2，成功 terminalize 的 cancellation 返回 130。目标进程非零 exit code 仍返回 0。

State 优先级是 `--state`、`RUNLAB_STATE`、`$XDG_DATA_HOME/runlab`、`~/.local/share/runlab`，布局为标准 OCI Image Layout 加 `runs.sqlite3`。State root、数据库、OCI metadata 和 blobs 以当前用户私有权限创建。OCI Layout 首次初始化和 index mutation 使用同一把布局锁；blob 在 digest namespace 之外的同文件系统目录 staging，再 no-clobber 发布。descriptor-backed reads 在同一 file descriptor 上完成 digest/size 验证和读取，避免 verify-close-reopen。SQLite 显式拒绝未知 storage version，且不会先用当前 schema 改写未来版本数据库。`runtime-config check` 与 `schema` 不解析 state。普通读取命令只进入已存在的 state，不会为了失败的 `image inspect`、`run get/list/diff`、stream read、authoring input resolution 或 VM operation query 创建目录或启动 VM。Image import/pull、Catalog mutation、Docker import 和 `run start` 是当前可以创建或扩展 state 的写路径；Image writer 才初始化 Layout，Run writer 才初始化数据库。

macOS `vm` 命令不使用上述 host state precedence，并显式拒绝 host `--state`。namespace grammar 固定为 1–63 个小写字母、数字、`-`、`_`，且首字符只能是小写字母或数字；guest rootful engine state 固定在 `/var/lib/runlab/namespaces/<namespace>`。Lima preflight 精确要求 limactl 2.2.0、VZ、同架构、plain mode、零 mounts 和当前 architecture 的 digest-pinned server image。create 输入是内置的单一 Ubuntu 24.04 release URL/digest，不使用 mutable alias 或 fallback；上游 release URL 的长期保留仍是 release artifact 风险。host/guest control protocol 固定为 v1，握手同时校验 RunLab package version、Linux OS 和 architecture；安装 RunLab/runc、输入和输出均校验 exact SHA-256/size，runc identity 精确要求 1.5.1、commit `v1.5.1-0-g8f2685a47`、Runtime Spec 1.3.0。公开 `vm exec` 只接受 public RunLab argv 和显式 `@input/N`、`@output/N` slot，不是 shell transport。

guest execution 使用 system systemd transient unit 持有 rootful installed RunLab process，SSH disconnect 不改变 operation state。transport metadata 和普通 staging 由 Lima guest user 持有；root-owned streams/output 只能由校验 UUID、kind 和 slot 后的 hidden control 经固定 sudo command 读取，不接受任意 guest path。operation UUIDv7 同时限定 metadata、staged files、unit name 和 control path；显式 `operation cancel` 发送 SIGINT，`get` 只读状态，`attach` 等待 terminal、回传独立 stdout/stderr 与 declared outputs，`discard` 只删除 terminal transport state。只有全部 host publication 和 stream write 成功后才移除 transport operation，失败则保留 identity 供恢复。

`vm exec --runtime-config-input N` 只对已声明的 OCI Runtime Config slot 做结构化 mount-source 改写。`mounts[*].source = "@input/M"` 被替换为 `/var/lib/runlab/vm-inputs/<operation>/source-M` 下 root-owned 0600 sealed file；helper 以 host 已记录的 digest/size 和 `O_NOFOLLOW` 重新校验 source，并要求重复 seal 的完整文件集合与 bytes 相同。Managed Service declaration 的 `runtime_config_file` 可以精确指向另一个 marked Runtime Config slot。complete、discard 和 abandon 都清理 transport 与 sealed roots。它不是任意 JSON/env substitution，也没有引入 Secret DSL。当前还没有持久 host-side operation catalog、自动 Linux artifact resolution、长期自有 VM image artifact 或 operation GC。

Run Database 公开 `run get`、有界的 `run list` 和结构化 `run diff`。List 按 UUIDv7 Run identity 倒序分页，默认 20、最多 100，可按 accepted/terminal lifecycle 过滤。Diff 排除两侧自己的 schema version、Run identity 与 accepted/terminal 时间，递归比较执行输入和结果事实；默认最多返回 200 个 field differences、最多允许 1000 个，并报告 total/truncated。两条读取路径只反序列化公开 Run Record JSON，不读取 runtime、stdin、stdout、stderr 的 SQLite BLOB，因此 stream 只以 availability、digest、size、limit 和 reason 参与比较。

OCI Distribution pull 已直接接入本地 OCI Layout。当前支持显式 registry/repository/reference、匿名/Bearer token flow、exact `linux/amd64|linux/arm64` selection、Manifest/Index/Config/Layer media types、descriptor size/digest 与 DiffID 验证，并在完整验证后更新 Catalog name。它没有 credential helper、push、retry policy、referrers/signature verification 或完整 registry compatibility matrix。

OCI-native `image import` 已接入只读 OCI Layout directory 和 plain tar archive。它从根 `index.json` 有界遍历 nested Index，只允许选择 reachable Manifest；`--source-reference` 精确匹配根 reference annotation，`--manifest` 提供互斥的 exact selector。缺失 descriptor platform 时读取 verified Config，descriptor 与 Config 矛盾时拒绝；显式 Manifest 不被无关候选的坏 Config 阻塞。source root/blob 通过 `NOFOLLOW` fd-relative reads 打开，archive 不展开到磁盘，拒绝路径逃逸、normalized duplicate、特殊 entry、sparse/global PAX、PAX size override、损坏 checksum、truncation、非零尾随数据和超限 graph/path/extension payload。Manifest、Config 和 Layer exact bytes 进入目标 Layout，renderer 完整验证 Layer path、whiteout、hardlink、PAX 和资源边界后才原子更新 Catalog；任一失败不移动现有 reference。source 与目标 Layout 重叠时在 state 初始化前拒绝，不修改 source tree。

## OCI Image 实现

已经实现：

- 从只读 OCI Layout directory 或 plain OCI archive 原生导入一个 reachable、verified OCI Image，不调用 Docker；
- 从 Docker 中已有的单一 native Linux image 导入完整 Config 和 ordered Layers；
- streaming SHA-256、descriptor digest/size、Manifest/Config/Layer media type、DiffID 和 platform 校验；
- 临时文件、`fsync`、no-clobber publish 与同 digest 现存内容复验；
- 受文件锁保护的 `index.json` read-modify-write；
- materialize 后校验 Docker rootfs DiffIDs；
- mutable checkout 与 Run container capture；
- Final Layer chain 必须延伸 Initial chain；Docker 返回 0-delta 时由 Image 层生成 deterministic empty tar Layer，因此每次成功 capture 都产生一个 child Manifest；
- Docker capture 先只读验证 archive Config、platform、Layer descriptors、DiffIDs 与 parent prefix，只 ingest 唯一新增 Layer；
- deterministic empty Layer 由 changeset encoder 生成固定 gzip bytes，不再由 capture 分支单独手写 tar；
- Final assembler 只接受一个已验证 Layer 与显式 capture time；相同输入产生相同 Config/Manifest；
- Final Config 保留 Initial Config 的未知字段，只追加一个 `rootfs.diff_ids` 与一条 RunLab history，不继承 Docker 临时 command、env、workdir 或 label；
- Final Manifest 保留 Initial Manifest 的 annotations、subject、artifactType、未知字段与已有 Layer descriptor objects，只替换 Config descriptor并追加一个 Layer；
- 直接从 verified plain/gzip/zstd OCI Layers 构造 byte-safe merged filesystem view，并原子提取一个 absolute regular file，不发现或调用 Docker；
- read path 处理 explicit/opaque whiteout、同层重建、forward hardlink chain、symlink resolution、type replacement、非 UTF-8 sibling、duplicate/unsafe path 和显式资源上限；hardlink resolution 使用非递归链压缩，不随链长产生递归栈或固定点重扫；
- PAX ordinal index 与 merged filesystem view 分别受 aggregate retained-byte budget 约束，不能通过大量小 PAX records、paths、xattrs 或 link targets 绕过单 entry 上限；
- output 使用同目录 staging、`fsync` 和 no-clobber publish，不覆盖普通文件、dangling symlink 或竞态中出现的目标。
- `image diff` 比较平台、Config、共同 Layer 前缀和 resolved filesystem，返回 added/removed/modified metadata；raw path 同时提供 escaped display 与精确 absolute `path_hex`，默认 100、最多 1000 条，并用 `--after-path-hex` 分页；
- `image export` 把 resolved filesystem 写成 deterministic plain tar，复用同一 Layer tar assembler，保留 byte paths、hardlinks、symlinks、devices、FIFO、metadata 与 xattrs，不携带已解析的 whiteout，也不覆盖现存输出。

changeset 内部 slice 已实现 raw-byte `FsPath`、semantic `Inventory`、before/after comparator、private content spool 与 deterministic gzip Layer encoder。普通修改直接编码为 OCI modification entry，只有真实删除产生 whiteout；directory 替换会压掉不可能应用的 descendant whiteout。Inventory 在 compare 前校验 parent directory、mode、timestamp、symlink NUL 与 hardlink anchor/content/metadata invariants；hardlink topology 变化会把稳定 raw-byte anchor 一起提升到新 Layer。

Layer encoder 已结构性覆盖 regular file、directory、symlink、hardlink、FIFO、character/block device、signed/subsecond PAX mtime、binary xattr、whiteout 与 deterministic empty Layer。link target 保留任意非 NUL bytes，长 target 使用 GNU long-link；device number 写入 tar header。共享的 length-aware PAX codec 不按换行切 value，可保留 newline/NUL，并同时写入 libarchive 兼容的 `SCHILY.xattr.*` raw value 与 `LIBARCHIVE.xattr.*` base64 value；两种表示冲突时 fail closed。renderer 通过 raw tar PAX ordinal index 读取相同 metadata，不再依赖 `tar` crate 0.4.46 的 line-splitting PAX iterator。

一个 regular-file changeset 已完成 `compare → encode → common Final assembler → Docker-free file get` round-trip。libarchive 3.7.4 已在显式 `--xattrs` 下独立解包并复验 regular bytes、binary newline/NUL xattr、hardlink inode、长/非 UTF-8 symlink target、FIFO 和 positive subsecond mtime。另一个 rootful Linux oracle 使用 `umoci 0.4.7` 独立解包由 RunLab 组装的三层 Final Image，再用同一 semantic Inventory 捕获结果；root/目录 metadata、raw-byte filename、addition、modification、explicit/opaque whiteout、hardlink、binary `user.*` xattr、FIFO 和 character device 1:3 均与预期一致。实测 libarchive 把 PAX `-0.5` 应用为 `+0.5`，而 umoci oracle 能还原该 timestamp；因此不能把单一 apply 工具当作所有 tar metadata 的通用 oracle。

Linux-only tree capture 已有首个实现：root/child directory fd 固定后使用 `openat/statat/fstat`，raw directory names 排序，regular bytes 在一次读取中 hash + private spool，并在读取前后复验 identity、stat 与 xattrs；symlink 通过 pinned parent 的 `/proc/self/fd` no-follow 路径读取 xattrs，FIFO/device 只读 metadata，socket fail closed，hardlink inode group 在完整 member 一致性校验后转为稳定 path anchor。Capture 在复制 path、分配 xattr buffer 和写 content spool 前消费整树 entries/path/xattr/content/depth budget，并要求两个完整 fd-relative capture pass 产生完全相同的 Inventory 才返回第二次的 content spool。它已在 Linux arm64、Rust 1.95 上通过编译、资源边界测试和上述真实 umoci apply-back 捕获；两个 agreeing walks 仍不是 filesystem 的原子 snapshot primitive。

Linux-only rootfs materializer 已接入 native Runner。它预扫描并验证 ordered Layers，在 recovery attempt 拥有的 private rootfs 内使用 fd-relative `NOFOLLOW` 操作应用 whiteout/opaque、type replacement、regular/directory/hardlink/symlink、binary xattr、FIFO/device。每个 Layer 的 regular content 只做一次有界线性 pass并逐文件落入私有 staging；forward hardlink 按依赖链解析，不做 O(N²) 固定点扫描。root/目录 metadata 跨 Layer 汇总后按深度逆序重放，因此 upper child 写入不会破坏 lower directory 的最终 mode/owner/time/xattr；同 Layer 中晚出现的 directory header 也不会删除已经出现的 child。recursive cleanup 有全局 entry/depth budgets。真实 Linux tests 通过 `TreeCapture` 严格比较多层结果，并独立覆盖 opaque whiteout、forward hardlink/cycle、cleanup limits 与 rootful character device 1:3。

Local Image Catalog 已实现完整的本地 reference lifecycle：reference identity 使用 index descriptor 的 `org.opencontainers.image.ref.name`，description/source/maintainer 使用 `io.runlab.catalog.*`，tag move 只替换同 key reference 并保留同 Manifest 的其他 aliases。`image catalog list` 返回有界、稳定排序的条目，`show` 解析并验证完整 OCI Image，`set` 在验证一个已存在的 Manifest 后创建或移动 reference，`remove` 幂等删除 reference 但不删除 content。`set` 可以设置或清除 description；省略时保留已有 description，移向新 Manifest 时 source/maintainer 收敛为 local provenance。省略 tag 使用 `latest`。

`run start`、`runtime-config create`、`image inspect`、`image diff`、`image export` 和 `image file get` 接受 Manifest digest 或本地 reference，miss 只在本地失败。使用 reference 的 Run Record 同时保存 normalized requested reference 和 acceptance 时固定的 Manifest descriptor，Primary 与 Managed Service 分别保留 provenance。首次 Layout 初始化和并发 Catalog mutation 有独立布局锁与并发测试。Image pull 可选写入 Catalog name，OCI-native import 要求显式 `--name`；Final publication 不自动生成 digest-derived reference。official provenance 与 Distribution push 仍未实现。

## State verify、retention 与 GC

`run verify RUN_ID` 在一个 SQLite deferred transaction 中交叉复验 stored lifecycle、accepted/terminal JSON、SQL projection、Runtime Config、stdin 和可用 streams 的 digest/size，然后验证该 Run 的每个 Initial 与 available Final Image graph。`state verify` 联合检查完整 OCI root Index、Catalog、全部 Run records、stored bytes、每个 rooted Image graph 和 digest namespace 中的每个 regular blob，同时报告 reachable/orphan bytes、staging entries 和 recovery entries。有效 orphan 是报告事实，不使 verify 自动删除或返回失败。

GC 只删除 OCI blob，不删除 Catalog entries、Run rows、recovery state 或 staging entries。Retention roots 包括根 `index.json` 的全部 Manifest descriptors（包括没有 Catalog reference annotation 的 entry）、每个 accepted/terminal Run participant 的 Initial Manifest，以及 terminal Run 的 available Final Manifest。`state gc plan --output FILE` 以 no-clobber 方式写入带 roots digest、类型化 roots、精确删除集和 plan digest 的 canonical JSON。`state gc apply PLAN` 验证 schema、plan/roots digest 与严格排序，重新读取并完整验证当前 state；新近变为 reachable 的 plan candidate 会跳过，plan 之后新出现的 orphan 不会被扩入删除集。所有当前 candidate 在第一次 unlink 前都重验 digest/size；重放同一 plan 把已删除 blob 报告为 `already_absent`。任何 accepted Run 或 recovery entry 存在时，plan/apply 均 fail closed。

State root 的 `.mutation.lock` 对普通 stateful operations 取 shared lease，对 GC plan/apply 取 exclusive lease。因此 GC 不会与 resolve/accept、blob/index publish、Catalog move/remove 或 Final publication 并发；普通 writers 之间仍使用 OCI index lock 和 SQLite transaction 来保护各自的 atomic mutation。

## Runtime Config 与 Docker profile

解析器拒绝 duplicate JSON keys，使用 `oci-spec` typed view 检查标准字段，再应用 RunLab backend-neutral 的 Linux Runtime 约束。Runtime config 保存完整 normalized JSON；OCI 合法但 Docker 不支持的字段不会在 `runtime-config check` 被误判为无效 OCI。

当前 RunLab Runtime 约束支持：

- OCI Runtime version `1.2.0`；
- `root.path=rootfs`、readonly root；
- non-terminal process、non-empty argv、exact env、absolute cwd；
- numeric uid/gid、additional gids、`noNewPrivileges`；
- hostname；
- exactly-once private pid/network/ipc/uts/mount/cgroup namespaces；
- native-only canonical read-only regular-file bind mounts；
- string annotations，其中 OCI stop-signal annotation 会映射到 Docker。

Docker container creation 显式映射 argv、env、cwd、user/groups、hostname、readonly root、no-new-privileges、network 和 cgroup namespace，并 drop all capabilities。pid 与 mount 使用 Docker container 固有隔离；当前没有逐字段 conformance test 来证明完整 OCI fidelity。Docker 仍注入其 default mounts 和 daemon policy；adapter 为当前 slice 使用 `seccomp=unconfined`，因此不能宣称支持任意 OCI Runtime Configuration。

Docker profile 位于 `docker/`，在 `run start` 的 Docker preflight、accepted transaction 之前转换并拒绝 adapter 无法 faithfully realize 的字段。named user、umask、PTY、mounts、resources、rlimits、capability set、seccomp profile、hooks、devices、masked/readonly paths 等当前会在这里作为 unsupported Docker capability 拒绝。Initial Image 的 declared Volumes 和删除 inherited environment name 也只属于 Docker preflight。

`runtime-config check` 是纯 OCI/RunLab structural operation，只返回 `schema_version`、`valid` 与 `oci_version`，不读取 state、不连接 Docker、也不声称某个 backend profile。独立 OCI bundle boundary 只接受已通过唯一键、OCI typed view 与 RunLab invariant 校验的 `RuntimeConfig`，没有第二条 raw JSON 解析路径；它创建私有临时 `rootfs/` 与 canonical `config.json`，拒绝 symlink/escape 并由对象生命周期清理。

Linux-only concrete `NativeBackend` 拥有 host、filesystem、network、resolver、rootful/rootless policy 与 Runtime Config realization；`RuncRunner` 只拥有 runc identity、subprocess lifecycle 与 raw observations。公开 `run start --backend native` 为每个 Run 使用 recovery attempt 内固定 runtime root 和由 Run ID 派生的 container ID，使用 pipe 并发 drain 独立 stdout/stderr，记录 retained bytes、observed bytes 与 partial fact，并实现 monotonic deadline 和 cancellation。正常路径在 stopped-state observation 后显式 `delete --force` 并审计 private root 为空；后置 observation、capture 或 cleanup 错误不会抹掉已经观察到的 process facts。

Native recovery attempt 位于 `<state>/recovery/native/<run-id>/`，以 0700 directory、0600 journal/lock/stream sidecar、进程锁、单调 phase、临时文件 + fsync + rename 保存资源身份和 terminal checkpoint。所有 native working directory 与 content spool 都位于该 attempt owner 下。`run get` 不触发恢复；监督器丢失后，`run reconcile RUN_ID [--dry-run]` 只做 orphan cleanup/finalization，不 reattach 或重启进程。无法证明的 process 时间、exit code 和 streams 保持 unavailable；`supervisor_lost` 记录为 operation/recovery error，不伪造成一个 Process Outcome。SQLite terminal transaction 成功后才删除 attempt；重复 reconcile 返回 `already_terminal`。

## Run lifecycle 与 Controls

两条 backend 共用 acceptance、Run facts、Final assembler 和 terminal transaction。native 路径的当前顺序是：

1. 读取、拒绝 duplicate keys、校验并规范化 Runtime Config。
2. 校验 Initial Manifest、Config、Layers、DiffIDs 和 Image defaults。
3. native preflight 验证 rootful Linux、runc identity、cgroup v2、实际 state filesystem 上的 OverlayFS profile 和受支持 Runtime Config。
4. 分配 UUIDv7 Run ID，先发布 recovery attempt，再在 `BEGIN IMMEDIATE` transaction 中写 accepted record 与 Runtime Config/stdin bytes。
5. 在 attempt workspace 中 materialize Initial Image、捕获 lower inventory、创建 bundle 并挂载 OverlayFS。
6. runc 直接消费 accepted `config.json`；supervisor 应用 timeout、stream limits、network 与 exact stdin，SIGINT/SIGTERM 请求有界 stop。
7. 持久化实际 process 与 stream facts；清理 runc state 和 runtime mounts。
8. 捕获 stopped merged filesystem，与 immutable lower 比较并由唯一 Image assembler 发布一个 child Layer 与 Final Manifest。
9. 卸载 OverlayFS，持久化 terminal checkpoint，再用一次 SQLite transaction 写 terminal record。
10. transaction 成功后删除 recovery attempt；异常中断由显式 reconcile 收束。

Controls 包括 exact stdin bytes、timeout seconds、stdout/stderr prefix limit 和 network request。native profile 支持 `network=none|egress`；Docker compatibility profile 只支持 `none`。`none` 要求 Runtime Config 自带 private network namespace；`egress` 要求省略该 namespace，由 RunLab 在 acceptance 之后创建并持久化一个 Run-owned namespace、确定性的 `/30` 地址计划、veth 和 nftables 规则。当前规则只允许 guest IPv4 source 经 FORWARD/NAT 到 `10.240.0.0/16` 以外的目标，丢弃宿主 INPUT/OUTPUT 与跨 Run pool 流量；两个 veth endpoint 在启用前关闭 IPv6 并 read back，因而没有 IPv6、port forwarding 或入站能力。DNS realization 从受支持的宿主 resolver 文件选择一至三个可路由 IPv4 nameserver，为每个 participant 安装带内容 digest/size 的临时只读 `/etc/resolv.conf` projection，并在 capture 前恢复原 target identity；缺失普通 target、symlink path、destination mount 冲突或无法证明 cleanup 时 fail closed。创建前需要 root、`ip`、`nft`、`conntrack`、`unshare`、`nsenter` 和 `net.ipv4.ip_forward=1`。allocator 在 host-wide lock 下只读取一次全部 IPv4 route tables，拒绝与候选 `/30` 重叠的 parent、exact 或 child route，并要求 guest-source conntrack 为空；lock、route snapshot 和全部候选共享一个总 deadline。cleanup 与 allocation 共用同一 host-wide lock，校验 exact veth alias/MAC/type 与 nft owner comment，不按可预测名称盲删，并按 nft、guest-source conntrack、veth 的顺序回收；迟到的 cleanup 若已没有 owned veth，会在删除 conntrack 前重新检查全路由表，已复用 subnet 时跳过删除。网络 cleanup 失败作为 terminal `resource_cleanup` fact 保存，recovery attempt 保留给后续显式 reconcile，不会让 Run 永远停留在 accepted。超过时间或 stream limit 会停止 container；stream slot 区分 `available|partial|unavailable|not_applicable`，partial 保存 prefix 的 digest、size、limit 和原因。轮询间隔内的突发输出可能使临时磁盘文件短暂超过 accepted limit，但持久化和内存读取只保留 prefix。

## Managed Service 与外部能力

当前 topology 只允许 zero or one required Managed Service，不提供通用 graph、restart policy、discovery、volume DSL 或 Kubernetes-like orchestration。声明文件只包含 `name`、`initial_manifest` Image selector、Runtime Config file 与 TCP readiness；字段名仍保留 `initial_manifest`，但当前值实际允许 Manifest digest 或 Local Catalog reference。Primary 和 Service 使用各自的 OCI Runtime Config；Managed topology 中两份 config 都省略 network namespace，由 RunLab 创建并记录一个共享私有 namespace identity。默认只启用 loopback；选择 `network=egress` 时，两个 participant 共享同一个 Run-owned outbound-only namespace。

执行顺序是 Service environment、Primary environment、Service process、readiness、Primary process、Service stop、分别 capture、shared network cleanup、one terminal SQLite transaction。Service 在 readiness 前退出时 Primary 为 `not_started`；Service 在 readiness 后、Primary 完成前退出时 Run 记录 `managed_service_lost` 并停止 Primary。两份 Initial/Final Manifest、Runtime Config、process/stream facts 和 operation errors 始终分开，数据库状态不会被并入 Primary Final Image。

标准 OCI mount 是当前最小外部能力入口。native profile 除固定 `/proc`、`/dev*` mounts 外，允许每份 Runtime Config 最多八个、整个 Run 合计最多八个 exact `[bind,ro,nosuid,nodev,noexec]` regular-file mounts。source 必须是规范绝对路径、无 symlink、位于 state 外、归当前有效 uid 所有、group/other 无权限且不超过 64 KiB。preflight 持有 source fd，start 前复验 path identity；destination 必须在 Initial Image 中预先存在为普通文件，runtime teardown 后也必须与挂载前 identity 相同，避免 runtime-created mountpoint 进入 changeset。RunLab 不读取或 hash source content，但目标进程可以主动把它复制到 stream 或 Final Image，因此这不是 exfiltration prevention 或 redaction feature。

当前真实 Agent 实验能力边界如下。它沿用 OCI Image、Runtime Config 和有界 topology，没有增加通用 Binding DSL：

| 实验需要 | 当前表达 | Run 后结果 |
| --- | --- | --- |
| 外部 API | `network=egress` | Network 是 capability；没有 Final Network，也不自动记录请求。RunLab 临时 projection 可路由 IPv4 resolver 配置，并在 Final capture 前移除 |
| 临时 credential | Runtime Config 中的受限只读 regular-file mount | Run Record 保存 source/destination reference，不读取或保存 secret value；目标主动复制后的 bytes 仍可能进入 stream 或 Final Image |
| 不可变小型资产 | 同一只读 regular-file mount | source 不变；当前不支持 directory mount，大型 dataset 应进入 Initial Image 或后续独立设计 |
| 可变数据库状态 | 一个 required Managed Service participant | Service filesystem 独立形成 `Service Image₀ → Service Image₁`，不会并入 Primary Final Image |

因此当前已经能做无网络任务、需要一个本地服务的任务、通过 IPv4 endpoint 使用外部 API 的任务，以及由只读文件注入 credential 的任务。它还不能把任意外部数据库 endpoint、read-write mount 或多个 service 的 before/after state 自动捕获成实验资产；这类状态不能被误报为 Primary Final Image 的一部分。

setup、capture 和 cleanup 的可恢复错误进入 `operation_errors`。进程已经启动后，process outcome 不会因 inspect/capture error 被改写为 `not_started`；缺少 exit code 或 start/end evidence 的终态不会伪装成 available Process facts。Final Config/Manifest content 按 digest 发布后即可作为 `final_image=available` 写入 terminal Run；Catalog mutation不是 Final publication 的隐式后续步骤。terminal database failure 或监督器强杀会留下可锁定的 accepted attempt；显式 reconciliation 停止仍可定位的资源、保存已有事实并以不完整终态封口，不透明重启目标进程。SQLite terminal transaction 已成功、但 recovery attempt 删除失败时，`run start` 返回 terminal record 加结构化 `cleanup.resources_absent=false/errors`，并返回非零 operation status；不会用一个 opaque 顶层错误隐藏已经提交的终态。

## SQLite 与公开 Schema

SQLite 当前使用 rollback journal `DELETE`、`synchronous=FULL`、5 秒 busy timeout 和 writer `BEGIN IMMEDIATE`。verify/retention snapshot 以 read-only connection 和 `BEGIN DEFERRED` 取一致视图，不初始化 schema 或创建 sidecar；WAL-mode database 在这条 read-only 维护路径上 fail closed。storage schema 带显式 version；打开未知未来版本时先拒绝，不执行当前 `CREATE TABLE IF NOT EXISTS`。accepted identity 包含 Initial Manifest、canonical Runtime Config 和全部 Run Controls；terminal update 先复验 accepted identity 与 stored byte digest/size，再一次性写入 terminal JSON 和 streams。

这些约束保护 RunLab 自身的状态转移，不是防篡改边界。拥有 state 文件的当前用户仍可以直接改写或删除 SQLite 记录。

`schema list|show` 为当前所有成功 JSON result shape 注册 schema：Run Record/start/list/diff/stream/reconcile/verify，OCI Image import/inspect/pull/Catalog list/show/set/remove/diff/export/file，Docker image import/checkout publish 共用的 `image-operation-result`、Docker materialize/checkout create，Runtime Config create/check、Managed Service check，state verify、GC plan document/plan result/apply result，managed VM status/install/operation/cancel/discard，以及有界的 schema list 本身。`schema list` 当前返回 36 个 kebab-case 名称；每个命令使用与 `schema show` 相同的 typed result，而不是独立拼装 `Value`。当前 error 仍是稳定 stderr diagnostic，不是 JSON error response；schema evolution 和兼容策略尚未冻结。

`BackendFacts` 已拆成 common `name/version/platform/network` 与 tagged `details`。`docker` 保存 context、endpoint kind 与 Engine ID；`native_linux` 保存 runc version/commit/runtime-spec、executable digest/size、kernel release、runtime invocation/config realization 与 filesystem realization。version text 不能唯一标识 runtime build；恢复必须重新观测相同 executable digest/size，否则 fail closed。rootful profile 为每个 participant 创建 dedicated cgroup，执行前记录 baseline，只有验证过 init membership 后才从 terminal `memory.events` 形成 OOM true/false；不能从 exit 9、137 或 SIGKILL 猜测。rootless restricted profile 不声称 cgroup 或 OOM facts。

## 尚未实现或未证明

- Catalog official provenance 与受信 metadata update flow；
- Distribution credentials/retry、push、referrers/signature verification 与完整 registry compatibility matrix；
- checkout remove 命令，checkout 依赖 image 内 `/bin/sh`；
- 持久 path index；
- Linux fd-relative capture 的 atomic snapshot boundary、`security.*`/`trusted.*` xattr 与 stale directory-xattr 行为，以及 OverlayFS upperdir decoder；
- 更多 crash phase、非 recovery-directory 的 host/runtime orphan discovery，以及更宽的普通 Linux distribution/kernel matrix；
- macOS managed VM 的自动 Linux artifact resolution、长期自有 VM image artifact、host operation catalog/GC，以及 transport-loss、disk-full、upgrade failure gates；
- 当前 exact memory limit/swap profile 之外的 resource constraints、除受限只读文件外的 mounts、PTY 和更完整 OCI Runtime fields；
- Docker default mounts、pid/mount namespace、security 与 daemon policy 的完整 fidelity 证明；
- Docker `stop` 与 attached child `wait` 的独立 wall-clock 上限；daemon 卡死时 Run 可能长时间留在 accepted；
- checkout commit 的 asset 已发布但 temporary tag cleanup 失败时，当前 CLI 仍返回 operation error，不输出已存在的 descriptor；
- streaming terminal SQLite write；当前 Docker path 先写有界临时文件，再把持久 prefix 读入内存；
- JSON error schema；
- Secret provider/version abstraction 与 redaction。受限只读文件 mount 只解决 ephemeral injection，不阻止目标主动复制；State、streams 和 Final Image 都应按敏感资产处理。

Experiment、Matrix、scoring、causal judgment 与跨 Run orchestration 是产品非目标，不是实现缺口。

## 迁移验证中发现并修正的问题

第一次真实 Docker 与 package 检查不是成功结果，暴露并修正了以下 defect：

1. 无效的 Docker `--pid private` flag 导致 accepted Run 无法启动。
2. Docker `OOMKilled` 被自动字段映射误读为 `OomKilled`。
3. Docker 的 0-delta commit 被误判为 capture failure；协议要求的 empty child Layer 一度又被错误实现为复用 Initial Manifest。
4. container 创建时没有打开 `OpenStdin`，accepted stdin bytes 没有进入目标 process。
5. 快速突发输出可以先退出、再绕过 capture-limit outcome。
6. 已发布的 Final Image 会被临时 Docker tag cleanup failure 覆盖成 unavailable。
7. Docker archive 的 Config DiffIDs 曾在 Manifest publish 后才通过 `inspect` 验证。
8. 旧 Python `__pycache__` 会进入 Cargo package，造成单一 Rust artifact 声明不成立。
9. Import/Final publication 自动写入 digest-derived Catalog reference，会让没有发现意义的名字无界增长；当前 Run asset 只保留 Manifest descriptor，Catalog mutation 改为显式操作。
10. OCI Layout 首次创建、blob staging、Manifest body mediaType 与 verify-close-reopen 边界存在竞态或混淆风险；当前使用初始化锁、独立 staging、same-fd verified read 和 descriptor/body 双重 mediaType 验证。
11. forward hardlink、跨层 directory metadata 和大量 PAX/view retained bytes 暴露递归栈、O(N²)、结果错误或资源上限缺口；当前用非递归 dependency resolution、最终 metadata replay 和 aggregate budgets 封闭。
12. 第一版 runc lifecycle 会因后置 observation/cleanup error 丢失已完成事实，regular-file stdout/stderr 可被目标 seek/覆写，cleanup failure 也缺少恢复身份；当前改用 pipes、per-stream facts、recoverable operation errors 和 runtime-root/container-id recovery handle。rootful fixture 的 bind mount cleanup 同时改为“卸载失败就保留 bundle”，避免临时目录清理穿过挂载点。
13. 第一次 supervisor SIGKILL fixture 发现 reconcile 会先发布缺少 process/stream facts 的 `cleanup_complete` journal；恢复路径现直接原子写完整 `terminal_prepared`，不暴露证据不完整的中间 phase。
14. 第二次 supervisor SIGKILL fixture 发现通用 Run TempDir 和 filesystem capture content spool 位于全局 `TMPDIR`，显式 reconcile 无法归属和清理；native path 的全部临时对象现统一位于 recovery attempt owner 之下。
15. runc cleanup 失败且后代进程继承 stdout/stderr pipe 时，旧实现会无限等待 reader thread。pipe 现为 nonblocking，drain 有独立 deadline；超时保留已观察 prefix、process facts 与 runtime recovery handle，而不是挂死或抹掉执行事实。
16. 同 phase 的 recovery API 曾可替换已经持久化的 process、stream、Final Image 或 terminal facts，reconcile 重试也会重复追加 `supervisor_lost`。这些 checkpoint 现在只允许首次写入或完全相同的幂等重放，错误列表按事实去重。
17. Primary 的 `RuntimeStartPending` 曾在 Service readiness 前过早持久化；runtime root 缺失也没有区分真正的 start-pending 与后续已清理 phase。checkpoint 现延迟到 readiness 成功后的实际 spawn 前，只有精确 `RuntimeStartPending` 且 root 缺失时 fail closed，`CleanupPending` 的缺失 root 视为已清理。
18. 第一版只读敏感文件 guard 只复验 path metadata，unlink/recreate 可能借 inode 复用绕过检查；destination 的 `O_PATH` fd 又曾被保留到 OverlayFS unmount，导致 cleanup 失败。source 与 destination 现在都固定原 fd identity，destination 在 capture/unmount 前完成复验并显式释放。
19. dirty worktree 中的 `cargo package` 曾把 4.1 GiB `target-linux` cache 和遗留 Python virtualenv 的同名 LICENSE/README 收进 package，生成物超过 1.1 GiB 后被中止。crate 现在使用 root-anchored package allowlist；加入 OCI ingress 后当前 package 只有 44 个声明文件、1.3 MiB，压缩后 261.5 KiB。
20. 第一版 merged tar export 复用了 captured-tree 的 strict `Inventory`，因此错误拒绝了 OCI Layer 中合法的 implicit parent directory。export 现直接从 resolved filesystem 构造 merged changeset，不把 capture-only parent invariant 强加给 Layer。
21. 第一轮 native default E2E 把 LinuxKit 的动态链接 `ip` binary 单独复制进隔离 PATH，丢失 `libbpf.so.1`，在 Managed Service network setup 前失败。产品代码没有为测试放宽；修正后的 verifier 在 Linux 容器安装标准 `iproute2` 后重跑通过。
22. Docker command demotion 后，真实 E2E 仍调用已删除的旧 `image import|checkout`，并把 native authoring helper 生成的标准 mounts 直接交给明确拒绝 mounts 的 Docker profile。当前测试使用公开 `docker image` namespace，并显式删除 mounts 来 author Docker-compatible config；两次失败均不计为通过。
23. Managed Service recovery 曾缺少与 Primary 对称的 native runtime identity guard；最终审查后两条 participant path 统一要求同一份可验证的 recovery identity。
24. 第一版 IPv4-only egress 规则没有关闭 veth IPv6，目标仍可能使用 link-local IPv6 绕开 IPv4 policy；当前在两个 endpoint 启用前关闭 IPv6，并验证 host sysctl readback 和 guest address absence。
25. 迟到的 cleanup 可能在 `/30` 已被新 Run 复用后删除新 Run 的 conntrack；allocation 和 cleanup 现在由同一 host lock 串行化，没有 owned veth 时还会在删除前重读完整 route snapshot。
26. egress allocation 持有 host lock 时，建网失败回滚曾再次获取同一把非重入锁，导致 5 秒假超时并掩盖原始错误；reservation 现在把已持有的 lock token 显式传给建网和回滚，进入普通 cleanup 前先释放 reservation，并有 Linux 回归测试覆盖持锁清理。
27. 第一版 OCI Layout/archive ingress 存在五个可观察缺口：显式 Manifest 会被无关损坏候选阻塞；state 初始化先于 source/destination overlap 拒绝并可能修改 source tree；transport PAX `size` 会造成预检与 tar parser 的边界分歧；目录成员在判断 regular file 前可能阻塞打开 FIFO；unsupported descriptor platform 会被错误折叠为 missing 并从 Config 重新解释。当前 exact selector 只 hydrate 选中候选，overlap 在 state 初始化前用 resolved future path 拒绝，archive 拒绝 PAX size override，Layout member 使用 `NONBLOCK|NOFOLLOW`，并独立保存 raw declared platform 到最终一致性校验。
28. GC 最初如果复用 Catalog view 作为全部 OCI roots，会遗漏根 Index 中没有 reference annotation 的 Manifest。当前 Layout 提供独立的全量 root inventory，Catalog 仍只暴露有 reference 的 entries；GC 使用前者。
29. 仅靠 OCI index lock 无法关闭 Catalog/Run roots 与 blob sweep 之间的竞态。当前 state-wide shared/exclusive lease 把全部普通 operations 与 maintenance 隔离，GC apply 还基于最新 roots 只缩小旧 plan 的删除集。
30. 维护路径需要不创建 sidecar 的 read-only SQLite snapshot，与先前文档所述 WAL 不相容。当前 writer 明确固定 rollback journal `DELETE`，read-only open 先检查 SQLite header 并对 WAL payload fail closed。
31. 早期有界 subprocess 只在 child leader 存活时计时；leader 退出后，继承 stdout/stderr pipe 的 descendant 可以让 reader thread 无界阻塞。当前 pipe 使用 nonblocking I/O，deadline 覆盖 child status、stream drain 和 thread join；超时后会停止 I/O 并回收 child，且有 inherited-pipe regression test。

中途失败和被并发源码修改污染的 package comparison 均不计入下方最终证据；全部修正后必须从稳定 worktree 重跑每个 gate。

早期四次 Claude Opus review 都在 5–10 分钟硬上限内没有输出，只能记为无证据的 `Execution error`，不能解释成“无问题”。后续两个有界源码审查 pass 成功返回，分别发现 Managed Service recovery guard 不对称、IPv6 policy bypass、迟到 cleanup 误删复用 conntrack，以及 allocation rollback 自锁四个高优先级问题，均已修复并进入真实 Linux 或定向回归验证。最终 pass 没有报告其他 blocker/high；它还指出 pre-acceptance 失败可能保留 attempt directory、reconcile discovery/open 之间存在一次性竞态，这两项不破坏已接受 Run 的事实或资源所有权，重试可收束，当前按低边际收益不扩张本 checkpoint。OCI ingress checkpoint 和本轮 schema-7 最终 checkpoint 各有一次 Claude Opus 5 review 在五分钟内没有输出，终止后返回 `Execution error`，因此同样没有审查证据。独立 subagent review 发现并推动修复了 checkpoint 可替换、reconcile 非幂等、runtime-start recovery 竞态、文档/公开 surface 不一致，以及上述 OCI ingress selector/overlap/PAX/FIFO/platform 五项缺口；最终 ingress 复核未发现剩余 blocker/high。审查只吸收影响正确性、安全、资源有界性、恢复能力或 contract honesty 的问题，低收益命名和风格意见不作为验收目标。

## 当前验证证据

2026-08-23 Asia/Shanghai，最终并发修改完成后的当前 worktree：

```text
macOS:
cargo fmt --check
cargo check --all-targets --all-features --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
→ passed

cargo test --all-targets --locked
→ unit 172 passed/2 ignored
→ CLI contract 21 passed
→ Image read integration 5 passed
→ OCI import/Catalog/GC integration 21 passed
→ backend/probe tests ignored by their declared environment gates

Linux arm64 container, Rust 1.95:
cargo +1.95.0 check --all-targets --all-features --locked
cargo +1.95.0 clippy --all-targets --all-features --locked -- -D warnings
→ passed; this is compile/lint evidence, not native/runc execution evidence

CARGO_TARGET_DIR=<isolated-temporary-target> cargo package --allow-dirty --locked
→ verified 71 files, 1.8 MiB, 369285-byte crate
→ archive contains native_backend/runc.rs, filesystem/pax.rs and image_ingress.rs

cargo install --path . --locked --root <temporary-root>
→ installed binary help/version/schema/invalid-command/missing-input separate-process checks passed
```

最新真实 Docker compatibility E2E 在当前代码路径上以 `alpine:3.22` 通过，耗时 48.01 秒，覆盖 exact streams、目标非零 exit、capture limits、timeout、SIGINT、Final Image 和 terminal SQLite bytes。Docker 不是 native 或 VM 证据。

一台 Debian 10 x86_64 普通用户开发机完成了当前 schema-7 static-musl binary 的 rootless control Run。冻结输入是 Pi 0.80.6 的单层 OCI Image Manifest `sha256:9418e6e70c576d57bb6d9eeff56d59a59f0c8a250a5c1dc4eaf44364b37ca66b`、Runtime Config SHA-256 `7bb10b963c58a3425604ec6c28a7275a9c61040eff00a661e6506b22d921f517`、`network=none`、60-second timeout 和 1 MiB stream limits。Run `run-01a02afd-71f2-7b21-99ff-fc333be56a89` 得到 exit 0、exact stdout `0.80.6\n`、empty stderr、Final Image available、零 operation error、`run verify`/`state verify` valid 与零 recovery residue；Terminal Run Record 保存了实际 runc artifact `sha256:df87472bcf881489d77480197f81339a14255fa470c594e1c3a05e5688401298` / 13509736 bytes。target process 只运行约 1.84 秒，但整个 350 MiB、包含大量 `node_modules` 的 materialize/capture 路径约需 6 分钟；这是当前 rootless Final Image 路径的性能边界，不能由 process 时间替代。

同一开发机还保留两个 pre-acceptance 失败 arm。官方 runc 1.5.1 amd64 artifact 在定制 kernel release `5.4.143.bsk.8-amd64` 上由 libpathrs kernel-version parser panic；不含 libpathrs 的同 source build 继续后，多层 Pi Image 因早期 Layer 存在非零 owner 而被 rootless materializer 拒绝，即使 resolved filesystem 后续已 chown 回 0/0。单层 flattened Image 是独立的新 arm，不是对失败评估器的追加修改。两次失败都未创建 accepted Run。

该主机只有 cgroup v1，没有 OverlayFS、nft/conntrack 或可用的 DeepSeek provider/model/credential；Pi 0.80.6 的真实非交互入口是 `pi -p`，但 rootless profile 明确禁止 egress 和 read-only credential mount。因此该结果只证明 Pi package 能在 RunLab rootless OCI Run 中执行和产生 Final Image，不是 DeepSeek Agent task 或 Managed Service 证据。

最新 Linux release binary SHA-256 为 `f0128ef92c7ccdd4eed7620cb10a483da2ffb8a26b08007925066c40c5192dd1`，在一个从固定 Ubuntu 24.04 arm64 image 新建、没有手工预配置的 Lima 2.2.0 VM 中安装并通过 runc 1.5.1 identity 和 reference-profile 检查。该 VM 上的最新 binary 复验了：

- Primary-only native Run：exit 0、Final Image available、零 operation error；
- detach/get/cancel/attach：active 状态可读，cancel 确认送达，attach exit 130，Process outcome 为 cancelled，Final Image available；
- host read-only file sealing：目标可读取 exact 22 bytes，写入得到 read-only error，Final target 与 Initial byte-identical，sealed/transport roots 清理；
- Managed Service：`--runtime-config=@input/0` 与 `--managed-service=@input/1` 通过真实 host/guest transport 重写，TCP readiness ready，Primary exit 0，Service 有界停止，两个 Final Image 独立；
- IPv4 egress：新的 root-owned filesystem allocation lock 下 DNS 与 HTTP 成功，Final Image 包含 559-byte `Example Domain` response，且用 standalone `@output/0` 取回后 digest/size 与 guest result 一致；
- `state verify`：valid，15 terminal Runs，accepted/staging/orphan blobs 全为零，recovery 中无 attempt。guest 残留审计也确认无 RunLab link、nft table、runc container 或 transport operation。

最终独立审查发现并修复了三个实质问题：既有 Lima instance 未精确校验内置 image URL/digest，Linux mountinfo 错误地假设全局 UTF-8，以及无权限的 abstract Unix socket allocation lock 可被本地普通用户抢占。现在分别改为 exact typed pin、byte-preserving mountinfo parser，以及 `/run/runlab/network-allocation.lock` 的 root-owned `0700/0600` filesystem lock。Linux 全测第一轮曾因新锁测试的 temporary-directory fixture 未设为 `0700` 失败；修正 fixture 后整套 Clippy 和 tests 重跑通过，第一轮不计成功证据。

真实 PostgreSQL 17 Alpine 三阶段实验仍是当前 Managed Service state transition 的较重证据：`DB₀ → DB₁ → restart(DB₁)`，查询 stdout exact `verified:initial\n`，数据库状态只进入 Service Final Image。它没有覆盖完整 crash/restart phase matrix。

一次 fresh egress 首次尝试在 acceptance 前被正确拒绝，因为原始 Alpine 缺少普通 `/etc/resolv.conf`。随后先产生包含空目标文件的 child Image，再以未改变的 egress contract 重跑并通过。该失败没有产生 Run，不能计作成功 arm。

## 历史验证证据

以下记录来自更早 checkpoint，用于保留缺陷和独立 probe 的 provenance；它们已被上面的当前门禁取代，不能作为最终 worktree 的单独通过证明。

2026-08-22 Asia/Shanghai 在 Catalog/verify/GC 修改之前的 Rust checkpoint 上运行：

```text
cargo fmt --check
→ passed

cargo test --all-targets
→ clean-target macOS: unit 125 passed/2 ignored, CLI contract 14 passed, image-read integration 5 passed, OCI-import integration 13 passed; backend/probe integrations ignored by default

cargo clippy --all-targets --all-features --locked -- -D warnings
→ passed

cargo +1.95.0 check --all-targets --locked
→ passed

cargo package --allow-dirty --locked --offline
→ packaged and verified 44 files, 1.3 MiB (261.5 KiB compressed)

cargo build --release --locked
→ passed

cargo install --path . --root <temporary-root> --locked
<temporary-root>/bin/runlab --help
<temporary-root>/bin/runlab schema list
→ installed binary and separate-process smoke test passed; current `image import --help` wrote 866 bytes and schema list wrote 576 bytes, with empty stderr

<temporary-root>/bin/runlab definitely-invalid
→ exit 2, 0 bytes stdout, 124 bytes diagnostic only on stderr

git diff --check
→ passed
```

Catalog/verify/GC 阶段在 fresh target 上的定向证据：

```text
cargo test --test cli_contract --test oci_import_cli
→ CLI contract 16 passed, OCI-import/Catalog/GC integration 18 passed; 0 failed
```

这组 separate-process tests 覆盖 Catalog set/move/description clear/remove 与 content retention，不存在 state 的 verify failure，orphan 报告，GC plan 内容与 digest，tampered plan 在删除前拒绝，apply/replay，新近 reachable candidate 跳过，以及 stale plan 不扩张到新 orphan。它们不替代修改后的 all-target、Clippy、MSRV、package、installed CLI 和真实 backend 全量重跑；在这些 gate 重新通过前，早期全量记录只是 regression baseline。

Linux arm64 / Rust 1.97.1 的冻结源码以 read-only mount 进入独立容器：

```text
cargo test --all-targets --all-features --locked
→ unit 137 passed/5 ignored, CLI contract 11 passed, image-read integration 3 passed; 0 failed

cargo clippy --all-targets --all-features --locked -- -D warnings
→ passed
```

该次并行运行覆盖 11 个普通 runc tests、4 个 read-only-file identity tests 与 6 个 reconcile tests，没有再出现测试 fixture executable 的 `ETXTBSY` 竞态。

真实 Docker rerun 使用本机 immutable image ID：

```text
Docker Server: 29.7.2 linux/arm64
RUNLAB_TEST_IMAGE=debian:bookworm-slim
cargo test --test docker_e2e -- --ignored --nocapture
→ 1 passed in 146.39s
```

这个 Docker test 通过真实 CLI subprocess 覆盖显式 `docker image import|checkout`、手工移除标准 mounts 后的 Docker-compatible Runtime Config、`egress` 在 acceptance 前拒绝、exact stdin、目标 process exit 7、exact stdout/stderr、快速突发输出的 partial capture、timeout、Final Image file extraction、SQLite terminal bytes、SIGINT 与 deterministic empty child Layer。它不证明 native runtime、recovery 或 OCI/Docker 完整 conformance。

真实 native CLI E2E 在 privileged LinuxKit / Linux arm64 / runc 1.3.6 上从同一冻结源码通过：

```text
RUNLAB_TEST_TMPDIR=/runlab-test
RUNLAB_TEST_RUNC=/proc/1/root/bin/runc
cargo test --test native_e2e native_cli_execution_contract --locked -- --ignored --nocapture
→ 1 passed in 12.94s
```

该 separate-process test 的全部 `run start` 都省略 `--backend`，因此直接验证 native default。它覆盖 binary stdin、exact stdout/stderr、exit 7、缺失 executable、timeout、1024-byte partial stdout、继承 pipe 的后代进程、SIGINT cancellation、Final exactly-one child Layer、Initial Layer prefix、一个 required Managed Service、共享 loopback network、readiness 和两参与者 capture。只读敏感文件使用两臂验证：trusted target 只读不复制时，内容不进入 stream、SQLite、OCI blob 或 Final Layer；主动复制时，内容确实进入 Final Image，证明该能力不是 exfiltration prevention。恢复场景在 runc init 已可观察时 SIGKILL supervisor，证明 `run get` 只读、reconcile dry-run 纯操作、显式 terminalization 与重复调用幂等，并审计 runc state、OverlayFS mount、network holder、attempt 和 scratch 全部为空。最终 Managed Service 输入收束后的 fresh verifier 为 1 passed，12.10 秒。

该最终 verifier 的第一条 setup arm 在执行测试前因 `sh -lc` 重置外部 PATH，以 `cargo: not found`、exit 127 结束；它不是产品测试结果。第二条 arm 只把 cargo 改成相同工具链的绝对路径，在同一源码和同一测试目标上完整通过；两条结果都保留在证据报告中。

第一次只读文件 rootful 运行暴露 destination guard 持有 OverlayFS fd 的真实 cleanup defect，不计为通过；修复后的一次运行又因测试 fixture 把 caller-owned credential source 误判为 RunLab 泄漏而失败，同样不计。最终运行由 fixture 在 cleanup 审计前删除自己拥有的 source，没有放宽生产资源检查。

第一次失败留下的临时 state 后来无法 reconcile，因为 panic 后的测试 `TempDir` 已部分删除 journal 引用的空 stderr sidecar；当前实现按完整性规则 fail closed。删除临时验证卷前的只读审计确认对应 runtime root、进程和 mount 均不存在。该残留不是成功恢复证据，也不计入通过结果。

native IPv4 egress packet contract 在 fresh privileged Linux verifier 上通过：

```text
iproute2 6.1.0 / nftables 1.0.6 / conntrack-tools 1.4.7
RUNLAB_TEST_RUNC=/proc/1/root/bin/runc
RUNLAB_TEST_IP=/usr/sbin/ip
RUNLAB_TEST_NFT=/usr/sbin/nft
RUNLAB_TEST_CONNTRACK=/usr/sbin/conntrack
cargo test --test native_e2e native_egress_packet_contract --locked -- --ignored --nocapture
→ 1 passed in 3.05s
```

该 separate-process test 使用另一个真实 network namespace 作为 outside target，覆盖三个场景。正常路径证明 guest 流量经过 FORWARD 并被 masquerade 为 host veth address，同时无法访问宿主 INPUT listener 或另一个可达的 Run address-pool target；Terminal Run Record 保存 namespace identity、guest/gateway/prefix facts，完成后 nft table、veth、holder 和该 guest source 的 conntrack entries 均为空。crash 路径在 active egress Run 上 SIGKILL supervisor，验证 host veth 的 IPv6 disable readback、guest `eth0` 没有 IPv6 address，并由显式 reconcile terminalize、清除全部 plan-owned resources。deferred-cleanup 路径把本 Run 的 nft table 替换为同名 foreign-owner table后停止 Run，验证 process cancellation status 仍为 130、Run Record terminal 且包含 `resource_cleanup`、attempt 保留；移除冲突后再次 reconcile 得到 `cleaned_terminal_attempt` 和 `resources_absent=true`。最终 fresh verifier 为 1 passed，7.38 秒；持有 allocation lock 的 rollback 定向单测为 1 passed，0.03 秒。相同 Linux 工作树的 all-target/all-feature Clippy 在 `-D warnings` 下零 warning。

第一轮暴露 nftables 1.0.6 的 JSON table listing 省略 comment，旧实现因此安全地拒绝删除自己的 table；修复后使用 JSON 校验 family/name、文本 listing 精确校验 owner comment。加入 conntrack 后的第一轮又暴露 conntrack-tools 1.4.7 不接受 `--list`，生产命令改为真实支持的 `-L/-D` 后才计为通过。两次失败均未被报告为成功。

真实 PostgreSQL 17 Alpine Managed Service 状态实验也从同一冻结源码通过：

```text
RUNLAB_TEST_POSTGRES_REMOTE=registry-1.docker.io/library/postgres:17-alpine
cargo test --test native_e2e postgres_managed_service_state_transition --locked -- --ignored --nocapture
→ 1 passed in 626.91s
```

它直接通过 OCI Distribution 拉取并验证 Image，第一轮启动 PostgreSQL 并捕获 `DB₀`，第二轮由 Primary 通过共享 loopback TCP 执行 SQL 后捕获不同的 `DB₁`，第三轮从 `DB₁` 重启 PostgreSQL并精确得到 stdout `verified:initial\n`。数据库状态始终属于 Service Final Image，没有并入 Primary Final Image；三轮均完成两参与者 capture、共享网络清理和一次 terminal SQLite transaction。

本机 libarchive apply probe：

```text
bsdtar 3.5.3 / libarchive 3.7.4
RUNLAB_TEST_BSDTAR=/usr/bin/bsdtar cargo test changeset::layer::tests::libarchive_applies_links_fifo_and_subsecond_mtime -- --ignored --nocapture
→ 1 passed
```

LinuxKit/runc lifecycle probe：

```text
Docker Desktop 4.86.0 / LinuxKit 6.12.76 / cgroup v2
runc 1.3.6 / runtime-spec 1.2.1
RUNLAB_TEST_RUNTIME_PROBE_IMAGE=3638d9a6fe40 cargo test --test oci_runtime_probe linuxkit_runc_subprocess_lifecycle_probe --locked -- --ignored --nocapture
→ 1 passed
```

该 probe 证明 binary stdin、独立 stdout/stderr、exit 0/7、fast exit、self-signal client status 133、TERM cancel 的目标 exit 42、deadline KILL status 137，以及 process tree、cgroup、runtime state 和 mounts 清理。OOM case 请求 `memory.limit=memory.swap=201326592`，实际得到 `memory.max=201326592`、`memory.swap.max=0`，并观测到 `memory.events.oom_kill` delta 1；status 137 本身不作 OOM 证明。`run --keep` 保留 stopped state/cgroup，显式 delete 后两者消失。

production-shaped `RuncRunner` 另在 privileged Linux/Rust 1.95 容器中直接使用 `/proc/1/root/bin/runc` 跑完整 rootful fixture：

```text
runc 1.3.6 / runtime-spec 1.2.1
RUNLAB_TEST_RUNC=/proc/1/root/bin/runc
RUNLAB_TEST_PYTHON=/usr/bin/python3
cargo test runc::tests::real_linuxkit_runc_1_3_6_production_lifecycle -- --ignored --nocapture
→ 1 passed
```

它覆盖 exact binary stdin/stdout/stderr、exit 0/7、fast exit、self-signal、TERM cancellation、deadline KILL、持续与快速 stdout-limit race、stopped-state observation、cgroup/runtime-root/bundle/mount cleanup。第一次容器命令因 login shell PATH 缺少 Cargo 而在测试启动前以 127 退出，该运行无 runc 证据；改用 Cargo absolute path 后上述 fixture 通过。结束后的命名容器、runtime root、bundle 和 host mount 审计为空。

上述 1.3.6 是早期 nested LinuxKit checkpoint 的历史 fixture，不是当前支持版本。当时官方 1.5.1 arm64 binary 在 Docker Desktop nested LinuxKit/fakeowner 边界中于 `fork/exec /proc/self/fd/6: permission denied` 失败；该结果只否定这个嵌套 verifier 组合。后续在非 nested-LinuxKit 的普通 Ubuntu VM 中验证后，当前代码精确要求 runc 1.5.1、commit `v1.5.1-0-g8f2685a47`、runtime-spec 1.3.0，preflight 拒绝不同 identity；当前 regression 结果见本节开头。

LinuxKit/Youki feasibility probe 固定官方最新 stable `v0.7.0`、commit `94ba653efbb180ce04650f6ae01a8e6bc8f96d92`。使用官方 `youki-0.7.0-aarch64-musl.tar.gz`，GitHub release API 与本地下载共同验证 archive SHA-256 `b96c05c2c82f1d20a74b611188fa120894c50a6128f73856bb371604ecb69bd0`，解包 binary SHA-256 为 `9acced77db02503fa397cca082aa3f0e60aa9410ed70cc69344d4682dbeccbf4`：

```text
RUNLAB_TEST_RUNTIME_PROBE_IMAGE=node:24-slim \
RUNLAB_TEST_YOUKI=/absolute/path/to/verified/youki \
cargo test --test oci_runtime_probe linuxkit_youki_v0_7_0_subprocess_lifecycle_probe --locked -- --ignored --nocapture
→ 1 passed
```

相同 nested Docker cgroup path 的两次冻结 corpus run 都在写入 case `cgroup.procs` 时返回 `EOPNOTSUPP`。改用 LinuxKit root-level 私有 cgroup 后，exact binary stdin/stdout/stderr、exit 0/7、fast exit、TERM cancel、deadline、两进程 process-tree 与 mount/cgroup/state cleanup 全部通过。Youki foreground `run` 对 self-signal 和 KILL 返回裸 client status 5/9，并在结束后删除 state 与 cgroup；`--keep` 没有保留它们，所以这些 status 不能单独证明 target exit/signal provenance。

OOM 仍为 `availability=unavailable`：OCI config 同时请求 `memory.limit=201326592` 与 `memory.swap=201326592`，Youki v0.7.0 接受该 config 后的实际 `memory.max` 是 `201326592`，但 `memory.swap.max` 仍为 `max`。目标通过 `crypto.randomFillSync` 触及并保留 1 GiB 随机页；probe 同时记录 `memory.current`、`memory.swap.current` 与 `oom_kill delta`，之后才显式 KILL 并验证 process/cgroup/state cleanup。因为 runtime 未落实有界的 swap 设置，该环境无法形成 bounded OOM gate；该结果不能支持 OOM 实现，也不能支持把 Youki foreground client status 直接写成 process fact。在改变 lifecycle boundary 或 runtime 版本并重跑之前，native Runner 继续不接线。

完整 Final Image 的独立 apply-back 在前序同一 changeset/assembler code path 上通过：

```text
Linux arm64 / umoci 0.4.7
RUNLAB_TEST_UMOCI=/usr/bin/umoci cargo test image::tests::umoci_applies_final_image_to_the_intended_semantic_inventory --locked -- --ignored --nocapture
→ 1 passed
```
