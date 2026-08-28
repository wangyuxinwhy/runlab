# 当前实现状态

本文只记录当前 worktree 的工程事实和剩余门禁。稳定产品、协议与架构由 [RunLab Agent Wiki](http://localhost:8787/app/pages/runlab-index--nw) 拥有。

## 已实现

Rust workspace 只包含 `run_protocol`、`run_engine` 和 `runlab` 三个 package，依赖方向为：

```text
runlab -> run_engine -> run_protocol
runlab ----------------> run_protocol
```

`run_protocol` 以 `ProgramInput.secrets` 表达精确的敏感环境变量和文件字节；`run_engine::NativeEngine` 在调用内交付它们，不拥有 Secret 来源或持久化。根 `runlab` package 已从 legacy 实现重写，不保留 Docker、managed service、recovery、reconcile、GC、schema、RunLab-specific runtime-config DSL、registry transport 或旧 Base/Overlay/Task 模型。

`runlab` 当前由八个直接模块组成：

| 模块 | 责任 |
| --- | --- |
| `cli` | 参数解析、命令分发、stdout JSON 与 stderr 错误边界 |
| `filesystem` | 从 Run Final Environment 或 Image 读取文件系统路径 |
| `image` | OCI Image Layout 导入、Catalog 查询与 Image 检查 |
| `managed_vm` | macOS 上固定 Lima/VZ Linux VM 的显式生命周期与兼容性检查 |
| `run` | Run identity、协议输入构造、NativeEngine 调用、结果投影与持久化 |
| `runtime_config` | 从 OCI Image Config 与固定 Linux 执行骨架生成标准 OCI Runtime Configuration |
| `state` | 本地 State 打开及组件装配 |
| `storage` | exact-byte OCI content store 与 SQLite catalog/Run records |

State CLI 只有以下八个命令：

```text
image import
image list
image get
filesystem get
run config generate
run start
run get
run list
```

macOS 另外提供 `vm create/start/install/stop/status`。它们只管理固定名为 `runlab` 的 Lima 2.2.0/VZ 实例，要求宿主架构匹配、plain mode、零 host mounts 和 digest-pinned Ubuntu Image。`vm status` 不改变 VM；其他操作均可安全重试。

`vm install` 从 macOS 可执行文件旁读取 `runlab-linux-<arch>` 与 `runc-linux-<arch>`，传输后复验 size 和 SHA-256，再以原子 rename 安装。开发构建可用 `RUNLAB_GUEST_BINARY` 与 `RUNLAB_GUEST_RUNC` 覆盖 bundle 路径。Guest 握手必须与 Host 的 RunLab version、transport version、Linux OS 和 architecture 完全一致。reference profile 当前固定 runc 1.5.1、`ip`、`iptables`、`ip6tables`、`nsenter`、cgroup v2、OverlayFS 与 IPv4 forwarding。

macOS 上所有 State 命令都在固定 Guest State `/var/lib/runlab` 执行，显式 `--state` 或 `RUNLAB_STATE` 会被拒绝。Host 只按已解析的命令类型构造 Guest 调用，没有公开或内部的任意 argv 转发入口。OCI archive、Runtime Configuration 和 stdin 先进入唯一 staging path，并在使用前复验 size 与 SHA-256。`run start` 由 transient systemd service 持有；Host 控制连接中断不会向 Run 发送取消，调用方可以用同一 `run_id` 查询已经持久化的状态。systemd 在 Run 结束后清理输入 staging。

`filesystem get` 当前只跨 VM 传回普通文件。Guest 与 Host 文件 identity 一致后才以 no-clobber 方式发布到请求的 macOS 路径，返回 JSON 中只出现该路径，不泄露 Guest staging path。

`image import` 和 `run start` 通过 `--description` 与可重复的 `--label KEY=VALUE` 接受总计不超过 8 KiB 的 Agent-facing metadata。Image metadata 属于可变 Catalog Entry，不改变 OCI digest；Run metadata 在 accepted 时固定，保存在 Run Record 中，不进入 Run Protocol 或 Engine。相应的 `list/get` 输出都会返回 metadata；按 digest 查询 Image 时没有唯一 Catalog Entry，因此 metadata 为 `null`。

`run config generate` 把完整 OCI Runtime Configuration JSON 写到 stdout，供 `jq` 等普通 JSON 工具继续处理。`run start` 省略 `--runtime-config` 时复用同一个生成器。生成器固定创建新的 network namespace，`isolated` 或 `egress` 仍只由 `run start --network` 选择，不写入 `config.json`。

`run start --secret-env NAME` 从调用方环境读取一个值，`--secret-file HOST_FILE=CONTAINER_PATH` 读取一个宿主文件。RunLab 在内存中构造 Protocol `Secrets`，公开 Run Record 只保存名称、目标和 `retained: false`；内部 identity 只保存内容摘要。NativeEngine 仅在私有调用 workspace 中派生 Runtime config 和只读 file mounts，Secret file 会在 Final Environment 捕获前移除。macOS transport 只传输 mode 0600 的临时 Secret 文件，不把值放入 argv，并在 transient systemd unit 结束后清理。

## State 与生命周期

State 目录包含 `oci/blobs/sha256`、`runlab.sqlite3` 和 Linux 执行时使用的 `engine` 目录。`engine` 内的 `invocations` 只保存调用期间的私有 workspace，正常返回后为空；`snapshots-v3` 保存由有序 DiffID chain 标识的只读 OverlayFS snapshot 与初始 filesystem Inventory。每次 Run 仍重新验证完整 OCI 输入；命中缓存只省去重复展开 Layer 和扫描初始 Inventory，每次调用使用独立 upperdir/workdir。OCI 内容按 Descriptor 的 size 与 digest 校验，并通过同目录临时文件原子发布。Image 名称只在完整 Manifest、Config、Layers 和 DiffIDs 验证后写入 Catalog。

Run identity 由调用者提供 canonical lowercase UUID v4。`run start` 在调用 Engine 前写入 accepted record；同一 identity、语义相同的输入与相同 metadata 返回已有记录，输入或 metadata 不同则拒绝。Engine 正常返回 `RunOutput` 或 `EngineError` 后写入 terminal completion。stdout 只返回有界摘要，包括 metadata、lifecycle、execution、各 Program 的 process、final environment 与 errors；stderr 同时输出以 `run.stream` 开始的实时 NDJSON，包含 `run.stage`、`program.stdout` 与 `program.stderr`。完整输入和持久标准流仍通过显式 `run get` 读取。

当前不实现跨进程恢复。进程在 accepted 之后、terminal 写入之前崩溃时，记录会诚实地保持 accepted 状态；没有 reconcile 或隐式重试。

## 已知边界

- `NativeEngine` 只在 Linux 可执行；macOS 通过 Managed VM 使用同一个 Linux binary 与 Engine，不存在 macOS Engine 实现。
- `NativeEngine` 支持 `Network::Isolated` 与 outbound-only `Network::Egress`。Egress 依赖宿主启用 IPv4 forwarding，并提供 `ip`、`iptables`、`ip6tables` 与 `nsenter`；Engine 不修改宿主级 forwarding 设置。
- Image import 只接受包含单个 Image Manifest 的标准 OCI Image Layout 目录或未压缩 tar archive。
- 支持 OCI tar、gzip 和 zstd Layer；不实现 registry pull 或 Image build。
- NativeEngine snapshot cache 当前没有公开命令、容量策略或淘汰机制；这些能力尚无使用场景，不在本次实现中。
- Native Linux 的 `filesystem get` 支持文件、目录和 symlink；macOS Managed VM transport 当前只传回普通文件。
- `filesystem get` 从 Run Program 的 Final Environment 或指定 Image 读取普通文件、目录或 symlink。目标路径必须尚不存在；单文件从最新 Layer 向前解析，目录只合并目标子树。
- stdout/stderr 既作为协议事实保存在完整 Run record 中，也在 `run start` 期间作为实时观察事件输出；独立 stream 命令尚未因真实场景而引入。
- 不实现恢复、验证、评分、golden comparison 或实验编排。

## 当前验证

macOS 与 Linux rootful VM 已通过：

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Rust 1.95 MSRV all-target check 已通过。独立进程 CLI 测试覆盖最小命令面、OCI Layout 导入、名称和 digest 查询、通过 Run 或 Image 读取文件系统路径、跨 Layer 目录合并、whiteout、opaque 目录、symlink、拒绝覆盖目标，以及错误请求不输出成功 JSON。

Image 与 Run metadata 已在 Managed Linux VM 中通过独立进程 CLI 测试：`image import` 写入的 description 和任意字符串 labels 会由名称形式的 `image get` 与 `image list` 返回，digest 形式查询返回 `metadata: null`；`run get/list` 返回持久 metadata；旧版 Catalog 与 Run 表会迁移并为既有记录补充空 metadata。相同 `run_id` 的 metadata 相等判断、8 KiB 上限、重复 label key、包含 `=` 的 value、macOS 参数转发和完整 CLI help 也有可执行覆盖。

真实 Linux CLI 纵切已通过 `runc 1.5.1`：导入 arm64 OCI Image，执行返回 exit 7 的 Run，保存独立 stdout/stderr 与 Final Image，通过 Final Image digest 提取 `/result/value` 的精确字节，并验证同 identity 重试返回 `created: false`。单独的长运行进程收到 SIGINT 后得到 terminal、`cancelled: true` 的 RunOutput，Engine workspace 无残留。实时观察纵切中，`run.stream`、`accepted`、`preparing` 在 0.107 秒到达，`executing` 在 0.242 秒到达，Program 首批 stdout/stderr 在 0.253 秒到达，Program 两秒后的第二段 stdout 在 2.251 秒到达，最终 stdout 摘要在 2.621 秒返回，证明事件没有等 Run 完成后批量输出。

紧凑 `run start` 结果也已通过真实 Linux CLI 验证：确定性 Program 分别写入 `compact-stdout` 与 `compact-stderr` 后 exit 7，命令返回 663-byte 摘要，保留 process 与 Final Environment 且不包含两个 stream payload；随后 `run get` 从完整 Record 精确恢复两段字节。同 identity 重试只把 `created` 改为 `false`，其余摘要一致。

真实 `NativeEngine` E2E 还覆盖了 `Network::Egress`：Program 从独立 OCI network namespace 主动连接 VM 上的 TCP 服务并取得响应；调用返回后临时 veth 与对应 IPv4/IPv6 firewall rules 均无残留。

Secret 纵切已从 macOS CLI 经 Managed VM 和真实 `runc` 验证：Program 同时读取一个 Secret 环境变量和一个只读 Secret 文件并退出 0；`run get` 只返回 `retained: false`，Final Environment 可以取得普通结果文件但无法取得 Secret file。NativeEngine opt-in real-runc 全生命周期测试也覆盖同样的 env/file 交付、Final Environment 排除和 workspace 清理。

Secret 环境变量还通过一次完整 macOS Agent User Story 验证：`pi + deepseek-v4-flash` 在 SWE-bench `psf/requests-5414` Image 中通过 `--secret-env DEEPSEEK_API_KEY` 完成任务，Program exit 0，无 execution/program error。`filesystem get --run` 取出的 571-byte `/artifacts/solution.patch` 与既有 golden patch 字节相同，SHA-256 均为 `adfa5771ae09b6ff1d91eb2a57943d20f0a899df777528a2233821e8f73fc20a`。

NativeEngine snapshot cache 与 upperdir-guided Final Image capture 已在同一 Managed VM 以 release binary 和 11-Layer SWE-bench Image 做 cold/warm 单次对照。snapshot chain 只发布 `upper`、`directories.bin` 和 `chain.bin`，解码后的 Layer 与 staged file 只存在于 build scratch；实际 cache 为 639 MiB，其中 Inventory 为 4.4 MiB，成功发布后没有 `build-*` 或 scratch 残留。cold `/bin/true` Run 从 accepted 到执行开始为 29.601 秒，warm Run 为 2.820 秒；对应完整命令 wall time 为 33.88 秒和 7.05 秒。进程结束到 terminal record 分别为 2.420 秒和 2.373 秒，两个 Run 均无 execution 或 Program error，Final Image digest 都是 `sha256:35f222e7175d8cc7bac5614f1fa0666d92ac7856d34d92c24859c11dc59dcd81`。另一次 warm Run 写入 5-byte `/artifacts/solution.patch`，wall time 为 7.13 秒，Final Image `sha256:9116f754d82d79aef469ba2f5cb4ed60afa0fd870591f2f1831c833c4fbf5f76` 可由 `filesystem get` 取回相同字节。调用后 invocation workspace 均为空。这些是固定环境中的各一次观测，不表示统计分布。

Runtime Configuration 生成能力已在同一 Linux VM 通过真实 CLI 纵切验证：`run config generate` 的精确 stdout 字节分别被省略 `--runtime-config` 的 `isolated` 和 `egress` Run 原样保存，两个 Run 都通过 `runc` 正常退出。生成的 JSON 包含新的 network namespace，但不包含 Run Protocol 的 `network` 字段。另一路径通过 `jq` 修改生成配置的 `process.args`，再以显式 `--runtime-config` 执行并取得预期 stdout。

`filesystem get --run` 已在同一 Linux VM 对真实 SWE-bench Run 验证。命令从最终 Image 取出 571-byte `/artifacts/solution.patch`，内容 digest 为 `sha256:adfa5771ae09b6ff1d91eb2a57943d20f0a899df777528a2233821e8f73fc20a`。最终代码的 release 构建首次观测为 27 ms，随后六次为 17–20 ms；debug 构建随后六次约为 179–180 ms。此前正序读取并重复校验全部 Layer 的实现稳定约为 30.8–31.0 s。

`cargo package -p run_protocol --no-verify --locked --allow-dirty` 成功。完整 workspace packaging 当前不能成立：`run_engine` 打包时会从 crates.io 解析 `run_protocol 0.1.0`，而该版本尚未发布。没有为绕过这一发布顺序去修改 manifest 或评价路径。
