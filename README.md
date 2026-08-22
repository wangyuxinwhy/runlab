# RunLab

RunLab 是建立在 OCI 标准对象之上的单机执行与资产记录工具。Primary-only Run 是：

\[
(\text{Primary Image}_0,\ \text{OCI Runtime config.json},\ \text{Run Controls})
\rightarrow
(\text{Run Record},\ \text{Primary Image}_1)
\]

当前实现允许额外绑定一个 required Managed Service participant。它拥有独立的 Initial Image、Runtime Config、process/stream facts 和 Final Image；只有这个 participant 自身 writable filesystem 中的状态会进入 Service Final Image：

\[
(\text{Primary Image}_0,\ \text{Service Image}_0)
\rightarrow
(\text{Run Record},\ \text{Primary Image}_1,\ \text{Service Image}_1)
\]

它不延续旧版 `Base + Overlay + Task` 模型，也不定义平行的 image 或 process DSL。OCI 拥有 Image、filesystem layer 和 Runtime Configuration；RunLab 拥有一次 Run 的接受、执行协调、事实记录和 terminal asset。

正式设计位于 Agent Wiki：

- [RunLab 设计索引](http://localhost:8787/app/pages/runlab-index--nw)
- [RunLab Run Protocol](http://localhost:8787/app/pages/runlab-core-model--gt)
- [OCI Image 与本地 Catalog](http://localhost:8787/app/pages/runlab-oci-image-catalog--jl)
- [系统设计](http://localhost:8787/app/pages/runlab-system-design--ly)
- [Execution Backend Contract](http://localhost:8787/app/pages/runlab-execution-backend--sv)
- [软件架构](http://localhost:8787/app/pages/runlab-software-architecture--np)

[IMPLEMENTATION.md](IMPLEMENTATION.md) 只记录当前 worktree 的实现事实和剩余风险；[ROADMAP.md](ROADMAP.md) 记录 Docker-free target 的实现顺序与 gates。

## Hello RunLab

Docker-free Hello 的 reference profile 需要 rootful Linux、Rust 1.95+、Cargo、cgroup v2 和 OverlayFS。native backend 精确要求 runc 1.5.1。普通 Ubuntu VM 上已经验证完整 rootful profile；普通非 root 用户也已验证受限 single-ID profile，但它只支持一个 participant、`network=none`、uid/gid 0、直接可写 materialized rootfs，并拒绝 resources、设备、特权 xattr、只读 host mount、egress 和 Managed Service。先构建单一 binary：

```bash
cargo build --release --locked
runlab=./target/release/runlab
```

直接从 OCI Distribution Registry 拉取一个与宿主架构一致的 Linux image：

```bash
$runlab --state ./state image pull \
  registry-1.docker.io/library/alpine:3.22 \
  --name alpine-hello
```

已有 OCI Image Layout directory 或 plain OCI tar archive 时，可以完全离线导入，不需要 Docker：

```bash
$runlab --state ./state image import ./alpine-layout \
  --name alpine-hello \
  --platform linux/amd64
```

多 image Layout 可用 `--source-reference` 精确匹配根 `index.json` 中的 `org.opencontainers.image.ref.name`，或用 `--manifest sha256:...` 选择一个从根 Index 可达的 Manifest；两者互斥。导入会验证完整 Manifest/Config/Layer graph、ordered DiffIDs 和 Layer filesystem semantics，验证成功后才创建或移动 `--name` 指定的本地 reference。source 只读打开，不能与目标 `state/oci` 重叠。

`--name alpine-hello` 会建立本地 `alpine-hello:latest` reference。可以先查看 Catalog，再把 Image Config 默认值显式转换为标准 OCI Runtime `config.json`：

```bash
$runlab --state ./state image catalog list
$runlab --state ./state image catalog show alpine-hello

$runlab --state ./state runtime-config create \
  alpine-hello \
  --output config.json
```

Catalog reference 是可变的本地发现信息，Manifest digest 才是 Image identity。可以显式把 reference 移到一个已经完整验证的本地 Manifest，或者只删除 reference；删除 reference 不删除 OCI content：

```bash
$runlab --state ./state image catalog set alpine-hello sha256:<manifest-digest> \
  --description 'Alpine image for local smoke tests'
$runlab --state ./state image catalog remove alpine-hello
```

`runtime-config create` 是 authoring helper。它从 Image Config 读取 `User`、`Env`、`Entrypoint + Cmd` 和 `WorkingDir`，生成一份可检查、可编辑的完整 OCI 1.2.0 Runtime Configuration；生成结果还显式包含 `rootfs`、writable root、`noNewPrivileges=true`、hostname、标准 mounts 和六个 private namespaces。它不是新的 Process Contract，也不是执行所必需的额外状态。`runtime-config check` 只验证 OCI/RunLab 结构约束，不证明某个 backend 能 faithfully realize 这份配置。

为本次 Run 修改命令并做纯校验：

```bash
jq '.process.args = ["/bin/sh", "-c", "printf \"Hello RunLab\\n\""]' \
  config.json > hello-config.json

$runlab runtime-config check hello-config.json
```

执行：

```bash
$runlab --state ./state run start \
  alpine-hello \
  --runtime-config hello-config.json
```

省略 `--platform` 时，pull 会选择当前宿主架构对应的 `linux/amd64` 或 `linux/arm64`。可选 Run Controls 包括 `--stdin FILE`、`--timeout-seconds`、`--stdout-limit-bytes`、`--stderr-limit-bytes` 和 `--network none|egress`。native profile 支持两种模式：`none` 由 OCI Runtime 创建 private network namespace；`egress` 由 RunLab 创建一个仅允许 IPv4 outbound forwarding/NAT 的 private namespace，关闭 veth IPv6，并拒绝访问宿主 INPUT 和其他 Run 的地址池。`egress` 从受支持的宿主 resolver 配置选择可路由 IPv4 nameserver，把规范化 `/etc/resolv.conf` 作为临时只读 projection 安装到 participant，并在 Final Image capture 前验证移除；Image 必须预先包含一个可安全覆盖的普通 `/etc/resolv.conf`。`egress` 要求 Runtime Config 省略 `linux.namespaces` 中的 `network` 项，并要求 rootful Linux 上存在 `ip`、`nft`、`conntrack`、`unshare`、`nsenter` 且 `net.ipv4.ip_forward=1`。Docker compatibility profile 仍会在 acceptance 前拒绝 `egress`，不会用普通 Docker bridge 近似。目标进程的非零 exit code 是执行事实，不会自动变成 RunLab operation failure。

读取 terminal record 或精确 stream bytes：

```bash
$runlab --state ./state run get <run-id>
$runlab --state ./state run list --limit 20
$runlab --state ./state run diff <left-run-id> <right-run-id>
$runlab --state ./state run stdout get <run-id> --output stdout.bin
$runlab --state ./state run stderr get <run-id> --output stderr.bin
```

`run list` 默认按 Run identity 倒序返回最多 20 条记录，可用 `--after` cursor 和 `--lifecycle accepted|terminal` 做有界查询。`run diff` 比较两条公开记录中的 input、controls、backend、participant、process、stream slot、Final Image 和 operation facts；它只读取记录里的 stream availability/digest/size，不读取或输出原始 stdout/stderr bytes，也不判断差异是否构成有效对照。

## macOS managed Linux VM

macOS 不直接运行 Linux OCI bundle，也不把宿主目录伪装成 Linux state。thin CLI 只管理一个同架构、`plain: true`、无 host mounts 的 Lima VZ VM；OCI Layout、SQLite、rootfs、runtime state 和 Final Image 都留在 guest disk。开发版需要先提供同版本的 Linux `runlab` binary：

```bash
runlab vm create --instance runlab
runlab vm start --instance runlab
runlab vm install --instance runlab \
  --binary /path/to/linux-aarch64/runlab \
  --runc /path/to/runc-1.5.1-linux-arm64
runlab vm status --instance runlab
```

`vm create` 精确要求 limactl 2.2.0，并使用内置于本版本 RunLab 的单一 Ubuntu 24.04 release URL、architecture 和 SHA-256，而不是 Lima 的 mutable Ubuntu alias 或无 digest fallback；默认 4 CPU、4 GiB memory、20 GiB disk。该 release URL 仍属于上游临时保留资产，后续 release 必须迁移到 RunLab-owned artifact provenance。当前 create 只创建 VM，不下载或构建 guest RunLab binary。`vm install` 同时要求 Linux RunLab 和 runc binary，对两者做传输 digest/size 校验，并拒绝不是 runc 1.5.1、commit `v1.5.1-0-g8f2685a47`、Runtime Spec 1.3.0 的 runtime。安装过程还会精确安装 Noble main 的 `conntrack=1:1.4.8-1ubuntu1`，分别原子发布 `/etc/modules-load.d/90-runlab-reference-profile.conf` 与 `/etc/sysctl.d/90-runlab-reference-profile.conf`，加载 OverlayFS，并立即应用和回读 `net.ipv4.ip_forward=1`。`vm status` 报告 canonical `ip`、`nft`、`conntrack`、`unshare`、`nsenter`、`cat`、`modprobe`、`systemd-run`、`systemctl` executable 与包版本，以及 cgroup v2、OverlayFS、systemd、当前和持久配置 facts；缺失 prerequisite 会得到 `ready=false`，不是被隐藏。`vm start` 会自动启动既有 VM，`vm exec` 也会在执行前验证同一 reference profile。执行要求显式 guest state namespace，拒绝 host `--state`：

```bash
runlab vm exec --instance runlab --namespace hello \
  --input image.tar -- \
  image import @input/0 --platform linux/arm64 --name hello

runlab vm exec --instance runlab --namespace hello \
  --output config.json -- \
  runtime-config create hello --output @output/0

runlab vm exec --instance runlab --namespace hello \
  --input config.json --input stdin.bin -- \
  run start hello --runtime-config @input/0 --stdin @input/1
```

`--input` 和 `--output` 按声明顺序对应 `@input/N`、`@output/N`。每个 output slot 必须精确引用一次；input slot 可以作为独立 argv token 或 `--option=@input/N` 的值直接引用，也可以由标记的 Runtime Config 或 Managed Service declaration 结构化引用，但不能重复直接引用。输入复制到 guest 后、输出发布到 host 前都复验 SHA-256 和 size；host output 使用 no-clobber 发布。transport 只转发公开 RunLab argv，不提供 guest shell passthrough，也不接受 `vm`、隐藏命令或转发的 `--state`。

当 Runtime Config 包含标准 OCI 只读 regular-file bind mount 时，host source 不能直接出现在 guest config 中。用 `@input/N` 作为 mount source，并用 `--runtime-config-input N` 标记需要结构化改写的 config slot；guest helper 会把对应 source seal 到 root-owned、0700 的 operation directory，以 0600 regular file 交给 rootful engine。该机制只改写 `mounts[*].source`，不做任意 JSON、环境变量或 Secret 替换。Managed Service declaration 的 `runtime_config_file` 也可以精确引用另一个已标记的 Runtime Config slot。

guest binary 在每次操作前完成 protocol、RunLab version、Linux OS 和 architecture handshake。transport metadata 和 staging 由 Lima guest user 持有，forwarded RunLab 则由 dedicated VM 内的 system transient unit 以 rootful Linux reference profile 执行，state 固定在 `/var/lib/runlab/namespaces/<namespace>`；因此 macOS 入口不会退化为拒绝 Managed Service 和 egress 的 rootless subset。SSH transport 中断不会定义取消或终止 Run。`vm exec --detach` 返回 operation identity；`vm operation get|attach|cancel|discard` 用于恢复、取回精确 stdout/stderr、显式发送 SIGINT，或删除无需取回的 terminal transport state。成功 attach 在 streams 和全部声明输出发布后才清理 transport operation；此前失败会保留 operation identity 供重试。

当前闭环已在一个从固定镜像创建、没有手工预配置的 Lima 2.2.0 / Ubuntu 24.04 arm64 VM 上通过 clean create/install、完整 stop/start、reference-profile 回读、OCI archive import、rootful native Run、exact stream、detach/cancel/attach、host read-only file sealing、一个 Managed Service、IPv4 egress、两个 participant 的独立 Final Image，以及 Final Image 再执行。最新末态 `state verify` 为 valid，accepted、staging、recovery 和 orphan blob 都是零。尚未完成自动选择 release artifact、transport-loss、disk-full、engine upgrade 和长期自有 VM image artifact gates，因此还不能把它称为完整 clean-host production gate。

## `--state` 是什么

`--state` 选择一组本机 RunLab 资产：

```text
state/
├── .mutation.lock
├── oci/
│   ├── oci-layout
│   ├── index.json
│   └── blobs/sha256/...
├── runs.sqlite3
└── recovery/native/<run-id>/...
```

它不进入执行身份，只决定本机从哪里解析 Manifest、向哪里写入 Final Image 和 Run Record。后续命令要读取同一份资产时必须选择同一个 state。选择优先级是：

1. `--state DIRECTORY`
2. `RUNLAB_STATE`
3. `$XDG_DATA_HOME/runlab`
4. `~/.local/share/runlab`

`runtime-config check` 和 `schema list|show` 是纯操作，不读取 state，也不要求 Docker 在线。`schema list` 有界列出当前全部成功 JSON result shapes，`schema show <name>` 返回对应命令实际使用的 typed result schema；operation error 仍写入 stderr，不属于成功 result schema。

可以分别验证一条 Run 保留的 bytes 与 OCI Images，或检查整个既有 state。`state verify` 把有效但不可达的 blobs 报告为 orphan，不会删除它们；这两条验证路径不会初始化缺失的 OCI Layout 或 Run Database：

```bash
$runlab --state ./state run verify <run-id>
$runlab --state ./state state verify
```

OCI blob 回收分为可审查的 plan 和显式 apply。GC roots 包括 OCI 根 Index 的全部 Manifest descriptors、所有 accepted/terminal Run 的 Initial Images，以及 terminal Run 中 available 的 Final Images。存在 accepted Run 或 recovery entry 时 GC 拒绝执行；apply 会重新计算最新可达性，跳过 plan 生成后新近可达的 content，也不会把新的 orphan 扩进旧 plan：

```bash
$runlab --state ./state state gc plan --output gc-plan.json
$runlab --state ./state state gc apply gc-plan.json
```

## 当前边界

`state/oci` 是标准 OCI Image Layout。`image import` 已实现只读 OCI Layout/plain archive 的 Docker-free ingress；`image pull` 已实现 OCI Distribution ingress、Bearer authentication 和精确 platform 选择。两条路径都校验 descriptor/blob、ordered DiffIDs 和 Layer filesystem semantics，并只在完整验证后更新 Catalog。Local Image Catalog 使用 `name:tag`，省略 tag 时固定解析 `latest`；`image catalog list|show|set|remove` 提供有界发现、本地解析和显式 reference lifecycle。`run start`、`runtime-config create`、`image inspect`、`image diff`、`image export` 和 `image file get` 都接受 Manifest digest 或本地 reference。reference miss 不访问网络，Run 在 acceptance 前固定 resolved descriptor，并把 requested reference 与 descriptor 一起保存。`run verify`、`state verify` 和两阶段 `state gc` 已实现内容校验与安全回收；official provenance、Distribution credentials、push、referrers 和 signature verification 尚未完成。

```bash
$runlab --state ./state image diff alpine-hello <other-image> --limit 100
$runlab --state ./state image export alpine-hello --output rootfs.tar
```

`image diff` 同时比较 OCI 结构和 resolved filesystem，raw Linux path 以可读转义和精确 `path_hex` 表达，并用 `--after-path-hex` 做有界分页。`image export` 生成 deterministic plain tar，不经 Docker，也不会覆盖已存在的输出文件。

`image import|inspect`、`runtime-config create` 和 `image file get` 直接读取 OCI content，不要求 Docker executable 或 daemon。`image import` 接受标准 OCI Layout/archive；Docker image-store 的兼容导入仍位于显式的 `docker image import`。Docker adapter 只通过 `docker image import|materialize|checkout` 和 `run start --backend docker` 提供兼容能力。`run start` 默认选择 native backend；当前需要 Linux 和精确支持的 runc identity，并直接执行 verified OCI content、由 RunLab 构造 Final Image。完整 reference profile 需要 rootful Linux；普通用户可以使用受限 rootless single-ID profile。请先运行 `runtime-config check`；完整支持矩阵和未验证语义见 [IMPLEMENTATION.md](IMPLEMENTATION.md)。

正式目标不要求用户安装 Docker：Image pull/read、file access、diff、render 和 Final Image construction 由 RunLab 直接基于 OCI content 完成；Linux reference execution 把 bundle 交给 OCI runtime，macOS 通过上述 managed local Linux VM transport 运行相同 engine。当前 native Linux 已贯通 materialize、rootless single-ID profile、runc、Final capture、一个 required Managed Service、outbound-only IPv4 egress 和显式 orphan reconciliation；macOS 已完成真实纵切但尚未通过 Phase 9 完整 production gate。Distribution credentials 与 push 尚未完成。

Managed Service 使用一个小型声明文件，只表达第二 participant 的 Image、Runtime Config 和 TCP readiness；它不是通用服务图。Primary 与 Service 的 Runtime Config 都省略 network namespace，由 RunLab 创建共享私有 namespace：`network=none` 只启用 loopback，`network=egress` 额外提供相同的 outbound-only IPv4 capability。Service readiness 成功后才启动 Primary；Primary 完成或 Service 提前退出都会触发有界停止，随后分别捕获两个 Final Image，并在一次 terminal transaction 中保存两组事实。

当前没有 Secret Binding 或 secret provider。native profile 仅提供一个窄能力：每份 Runtime Config 最多八个、整个 Run 合计最多八个标准 OCI read-only regular-file bind mounts；source 必须是 state 外、当前执行用户私有、最多 64 KiB 的规范绝对路径，destination 必须是 Initial Image 中已存在的普通文件。RunLab 保存 accepted Runtime Config 中的 source/destination 引用，但不读取、hash 或记录 source 内容。它不是 redaction 或 exfiltration prevention；目标进程可以主动复制内容，因此 streams、RunLab state 和 Final Images 仍应按敏感资产处理。

native Run 的协调器若异常消失，`run get` 只返回已保存事实，不隐式接管进程。管理员在确认原监督器已经退出后显式执行：

```bash
$runlab --state ./state run reconcile <run-id> --dry-run
$runlab --state ./state run reconcile <run-id>
```

reconcile 不会重新执行目标命令。accepted orphan 会停止仍可定位的资源、尽力保存已有事实并以 `supervisor_lost` 形成不完整终态；已经 terminal 但 cleanup 延后的 Run 只继续清理 recovery attempt，不修改 Terminal Run Record。

## 开发验证

```bash
cargo fmt --check
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo +1.95.0 check --all-targets --locked
```

默认 native backend 的真实 rootful Linux 测试标记为 ignored，显式运行：

```bash
RUNLAB_TEST_RUNC=/absolute/path/to/runc \
  cargo test --test native_e2e native_cli_execution_contract -- --ignored --nocapture
```

native egress packet contract 还需要显式提供真实网络工具：

```bash
RUNLAB_TEST_RUNC=/absolute/path/to/runc \
RUNLAB_TEST_IP=/absolute/path/to/ip \
RUNLAB_TEST_NFT=/absolute/path/to/nft \
RUNLAB_TEST_CONNTRACK=/absolute/path/to/conntrack \
  cargo test --test native_e2e native_egress_packet_contract -- --ignored --nocapture
```

真实 Docker compatibility 测试同样标记为 ignored，显式运行：

```bash
RUNLAB_TEST_IMAGE='sha256:<local-linux-image-id>' \
  cargo test --test docker_e2e -- --ignored --nocapture
```
