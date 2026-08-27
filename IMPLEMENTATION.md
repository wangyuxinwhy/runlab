# 当前实现状态

本文只记录当前 worktree 的工程事实和剩余门禁。稳定产品、协议与架构由 [RunLab Agent Wiki](http://localhost:8787/app/pages/runlab-index--nw) 拥有。

## 已实现

Rust workspace 只包含 `run_protocol`、`run_engine` 和 `runlab` 三个 package，依赖方向为：

```text
runlab -> run_engine -> run_protocol
runlab ----------------> run_protocol
```

`run_protocol` 和 `run_engine::NativeEngine` 保持原有稳定边界。根 `runlab` package 已从 legacy 实现重写，不保留 Docker、managed VM、recovery、reconcile、GC、schema、RunLab-specific runtime-config DSL、registry transport 或旧 Base/Overlay/Task 模型。

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

macOS 另外提供 `vm create/start/stop/status`。它们只管理固定名为 `runlab` 的 Lima 2.2.0/VZ 实例，要求宿主架构匹配、plain mode、零 host mounts 和 digest-pinned Ubuntu Image。`vm status` 不改变 VM；create/start/stop 均可安全重试。当前还没有 Guest 安装、握手、命令转发或文件传输。

`run config generate` 把完整 OCI Runtime Configuration JSON 写到 stdout，供 `jq` 等普通 JSON 工具继续处理。`run start` 省略 `--runtime-config` 时复用同一个生成器。生成器固定创建新的 network namespace，`isolated` 或 `egress` 仍只由 `run start --network` 选择，不写入 `config.json`。

## State 与生命周期

State 目录包含 `oci/blobs/sha256`、`runlab.sqlite3` 和 Linux 执行时使用的 `engine` workspace。OCI 内容按 Descriptor 的 size 与 digest 校验，并通过同目录临时文件原子发布。Image 名称只在完整 Manifest、Config、Layers 和 DiffIDs 验证后写入 Catalog。

Run identity 由调用者提供 canonical lowercase UUID v4。`run start` 在调用 Engine 前写入 accepted record；同一 identity 与语义相同的输入返回已有记录，输入不同则拒绝。Engine 正常返回 `RunOutput` 或 `EngineError` 后写入 terminal completion。命令只返回有界摘要，包括 lifecycle、execution、各 Program 的 process、final environment 与 errors；完整输入和标准流仍通过显式 `run get` 读取。

当前不实现跨进程恢复。进程在 accepted 之后、terminal 写入之前崩溃时，记录会诚实地保持 accepted 状态；没有 reconcile 或隐式重试。

## 已知边界

- `NativeEngine` 只在 Linux 可执行；macOS 已能显式管理本地 Linux VM 生命周期，但普通 State 命令尚未转发到 VM，不能直接开始 Run。
- `NativeEngine` 支持 `Network::Isolated` 与 outbound-only `Network::Egress`。Egress 依赖宿主启用 IPv4 forwarding，并提供 `ip`、`iptables`、`ip6tables` 与 `nsenter`；Engine 不修改宿主级 forwarding 设置。
- Image import 只接受包含单个 Image Manifest 的标准 OCI Image Layout 目录或未压缩 tar archive。
- 支持 OCI tar、gzip 和 zstd Layer；不实现 registry pull 或 Image build。
- `filesystem get` 从 Run Program 的 Final Environment 或指定 Image 读取普通文件、目录或 symlink。目标路径必须尚不存在；单文件从最新 Layer 向前解析，目录只合并目标子树。
- stdout/stderr 当前作为协议事实保存在完整 Run record 中；独立 stream 命令尚未因真实场景而引入。
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

真实 Linux CLI 纵切已通过 `runc 1.5.1`：导入 arm64 OCI Image，执行返回 exit 7 的 Run，保存独立 stdout/stderr 与 Final Image，通过 Final Image digest 提取 `/result/value` 的精确字节，并验证同 identity 重试返回 `created: false`。单独的长运行进程收到 SIGINT 后得到 terminal、`cancelled: true` 的 RunOutput，Engine workspace 无残留。

紧凑 `run start` 结果也已通过真实 Linux CLI 验证：确定性 Program 分别写入 `compact-stdout` 与 `compact-stderr` 后 exit 7，命令返回 663-byte 摘要，保留 process 与 Final Environment 且不包含两个 stream payload；随后 `run get` 从完整 Record 精确恢复两段字节。同 identity 重试只把 `created` 改为 `false`，其余摘要一致。

真实 `NativeEngine` E2E 还覆盖了 `Network::Egress`：Program 从独立 OCI network namespace 主动连接 VM 上的 TCP 服务并取得响应；调用返回后临时 veth 与对应 IPv4/IPv6 firewall rules 均无残留。

Runtime Configuration 生成能力已在同一 Linux VM 通过真实 CLI 纵切验证：`run config generate` 的精确 stdout 字节分别被省略 `--runtime-config` 的 `isolated` 和 `egress` Run 原样保存，两个 Run 都通过 `runc` 正常退出。生成的 JSON 包含新的 network namespace，但不包含 Run Protocol 的 `network` 字段。另一路径通过 `jq` 修改生成配置的 `process.args`，再以显式 `--runtime-config` 执行并取得预期 stdout。

`filesystem get --run` 已在同一 Linux VM 对真实 SWE-bench Run 验证。命令从最终 Image 取出 571-byte `/artifacts/solution.patch`，内容 digest 为 `sha256:adfa5771ae09b6ff1d91eb2a57943d20f0a899df777528a2233821e8f73fc20a`。最终代码的 release 构建首次观测为 27 ms，随后六次为 17–20 ms；debug 构建随后六次约为 179–180 ms。此前正序读取并重复校验全部 Layer 的实现稳定约为 30.8–31.0 s。

`cargo package -p run_protocol --no-verify --locked --allow-dirty` 成功。完整 workspace packaging 当前不能成立：`run_engine` 打包时会从 crates.io 解析 `run_protocol 0.1.0`，而该版本尚未发布。没有为绕过这一发布顺序去修改 manifest 或评价路径。
