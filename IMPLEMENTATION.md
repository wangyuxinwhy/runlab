# 当前实现状态

本文只记录当前 worktree 的工程事实和剩余门禁。稳定产品、协议与架构由 [RunLab Agent Wiki](http://localhost:8787/app/pages/runlab-index--nw) 拥有。

## 已实现

Rust workspace 只包含 `run_protocol`、`run_engine` 和 `runlab` 三个 package，依赖方向为：

```text
runlab -> run_engine -> run_protocol
runlab ----------------> run_protocol
```

`run_protocol` 以 `ProgramInput.secrets` 表达精确的敏感环境变量和文件字节；`run_engine::NativeEngine` 在调用内交付它们，不拥有 Secret 来源或持久化。根 `runlab` package 已从 legacy 实现重写，不保留 Docker、managed service、recovery、reconcile、GC、RunLab-specific runtime-config DSL、registry transport 或旧 Base/Overlay/Task 模型。

`runlab` 当前由十五个直接模块组成：

| 模块 | 责任 |
| --- | --- |
| `cli` | 参数解析、命令分发、stdout JSON 与 stderr 错误边界 |
| `docs` | 随当前二进制发布的 version-matched Markdown topic registry |
| `filesystem` | 从 Run Final Environment 或 Image 读取文件系统路径 |
| `image` | OCI Image Layout 导入、Catalog 查询与 Image 检查 |
| `managed_vm` | macOS 上固定 Lima/VZ Linux VM 的显式生命周期与兼容性检查 |
| `metadata` | description 与任意字符串 label 的有界解析与校验 |
| `observation` | `run start` 与 `exec` 期间实时 stderr NDJSON 事件的编码 |
| `public_schema` | 稳定公共 `runs` SQL Relation 及可发现 schema |
| `query` | 有行、cell、总输出和时间边界的只读 SQL 执行 |
| `run` | 协议输入构造、NativeEngine 调用，以及持久 Run 的 identity、结果投影与持久化 |
| `runtime_config` | 从 OCI Image Config 与固定 Linux 执行骨架生成标准 OCI Runtime Configuration |
| `state` | 本地 State 打开及组件装配 |
| `storage` | exact-byte OCI content store 与 SQLite catalog/Run records |
| `storage_management` | State 容量、资产引用与安全回收计划 |
| `error` | 稳定的结构化 CLI 错误封装与跨 VM 保真 |

仓库中的 `images/` 保存外部 Docker Buildx 使用的 Agent Image 构建源，不属于 Rust package 或 RunLab Image Builder。当前 `base` target 以 digest-pinned Ubuntu 24.04 为共同前缀，分层加入系统/native build 工具、Python 3.12、Node.js 24.20.0、uv 0.12.7 和固定的 `agent:1000:1000` 用户目录契约。`pi`、`claude` 与 `codex` 从同一 base 派生；`all` 继续复用完整 Pi Layer，再加入 Claude Code 与 Codex CLI。五个 target 都有从标准输入交给 bash 的真实 Run smoke 程序。

State-dependent CLI 当前包含：

```text
image import
image list
image get
image export
filesystem get
filesystem changes
exec
run config generate
run start
run cancel
run get
run list
schema list|get
query run
storage status
storage prune check|apply
```

此外，`docs list/get` 在打开 State 或连接 Managed VM 之前本地执行。`start-here`、`how-to/build-images` 与 `how-to/query-runs` 由普通 Markdown source、编译期 `include_str!` registry 和薄 CLI adapter 组成；Root Help 明确指向完整首次工作流，`list` 返回带 `schema_version` 的紧凑 JSON，`get` 默认返回 Markdown并支持 `--output json`。当前没有引入 `docs search`。

macOS 另外提供 `vm create/start/install/stop/status`。它们只管理固定名为 `runlab` 的 Lima 2.2.0/VZ 实例，要求宿主架构匹配、plain mode、零 host mounts 和 digest-pinned Ubuntu Image。`vm status` 不改变 VM；其他操作均可安全重试。

`vm install` 从 macOS 可执行文件旁读取 `runlab-linux-<arch>` 与 `runc-linux-<arch>`，传输后复验 size 和 SHA-256，再以原子 rename 安装。开发构建可用 `RUNLAB_GUEST_BINARY` 与 `RUNLAB_GUEST_RUNC` 覆盖 bundle 路径。Guest 握手必须与 Host 的 RunLab version、transport version、Linux OS 和 architecture 完全一致。reference profile 当前固定 runc 1.5.1、`ip`、`iptables`、`ip6tables`、`nsenter`、cgroup v2、OverlayFS 与 IPv4 forwarding。

macOS 上所有 State 命令都在固定 Guest State `/var/lib/runlab` 执行，显式 `--state` 或 `RUNLAB_STATE` 会被拒绝。Host 只按已解析的命令类型构造 Guest 调用，没有公开或内部的任意 argv 转发入口。OCI archive、Runtime Configuration 和 stdin 先进入唯一 staging path，并在使用前复验 size 与 SHA-256。`run start` 与 `exec` 都由 transient systemd service 持有；前者在 Host 控制连接中断后仍可用同一 `run_id` 查询持久状态，后者没有身份、记录或恢复读取面。systemd 在调用结束后清理输入 staging。

`filesystem get` 在 Guest 物化请求节点后，将文件、目录或 symlink 封装成只含一个 `payload` 的临时 archive。Host 校验 archive identity、路径边界、节点类型以及普通文件的 digest/size，再以 no-replace rename 发布到请求的 macOS 路径；返回 JSON 中只出现该路径，不泄露 Guest staging path。

macOS Runtime Configuration 中除 RunLab 固定 resolver scaffold 外的 bind source 被解释为 macOS Host 路径。只读普通文件和目录按校验过的 bytes/archive 暂存到 VM，并通过 transient systemd unit 的 `BindReadOnlyPaths` 映射到配置中未修改的绝对路径；每次执行拥有私有 mount namespace，结束时同时清理 staging。这样 Guest 接受、Engine 执行和 Run Record 保存的仍是调用方原始 Runtime Configuration。未含 `ro` 的 Host bind mount 在接受 Run 前拒绝，避免产生不会同步回 Host 的伪 writable mount。

CLI 失败统一写入 schema version 1 的 `runlab.error` JSON。已知输入错误、资源不存在、身份冲突、Managed VM 不可用与内部错误分别使用稳定 category；`accepted` 和 `run_created` 在无法证明时为 `null`。Guest 的结构化错误由 macOS transport 识别并保持字段不变；streaming 调用已经转发的错误不会在 Host 再输出一次。

public `runs` Relation 直接从不可变 Run JSON 派生 primary Program 的 started/ended、执行毫秒数、acceptance→start、end→terminal、retained stdout/stderr bytes 与 Final Image digest。它不复制流内容或完整 Final Environment。

`image export` 把 Catalog Image 或一个 Run Program 的 Final Image 写成标准未压缩 OCI Image Layout archive。导出逐个流式复验 Descriptor size 与 SHA-256，以同目录临时文件和 no-replace rename 发布，不覆盖既有路径；macOS transport 还会复验整个 archive 的 size 和 digest。

`image import` 和 `run start` 通过 `--description` 与可重复的 `--label KEY=VALUE` 接受总计不超过 8 KiB 的 Agent-facing metadata。Image metadata 属于可变 Catalog Entry，不改变 OCI digest；Run metadata 和调用方选择的 Initial Image Catalog name 在 accepted 时固定，保存在 Run Record 中，不进入 Run Protocol 或 Engine。相应的 `list/get` 输出都会返回 metadata；按 digest 查询 Image 时没有唯一 Catalog Entry，因此 metadata 为 `null`。

`run list` 只是默认 10 条的紧凑选择视图，展示到整秒的 UTC 时间；`run get` 保留完整 Run 事实与原始时间精度。需要组合筛选时，`query run` 只向可发现的公共 `runs` Relation 执行单条只读 SQL；私有表、SQLite schema、写操作、扩展和外部数据库都被拒绝。输出受 row、cell、总字节和时间上限约束，截断会显式返回 `complete: false` 及原因。`schema list/get` 是该公共 Relation 的唯一 schema 发现面。

`run config generate` 把完整 OCI Runtime Configuration JSON 写到 stdout，供 `jq` 等普通 JSON 工具继续处理。`run start` 与 `exec` 省略 `--runtime-config` 时复用同一个生成器。生成器固定创建新的 network namespace，`isolated` 或 `egress` 仍只由执行命令的 `--network` 选择，不写入 `config.json`。

`run start` 与 `exec` 的 `--secret-env NAME` 从调用方环境读取一个值，`--secret-file HOST_FILE=CONTAINER_PATH` 读取一个宿主文件。RunLab 在内存中构造 Protocol `Secrets`；只有持久 Run 的公开 Record 保存名称、目标和 `retained: false`，内部 identity 只保存内容摘要。NativeEngine 仅在私有调用 workspace 中派生 Runtime config 和只读 file mounts，Secret file 会在可选 Final Environment 捕获前或执行清理时移除。macOS transport 只传输 mode 0600 的临时 Secret 文件，不把值放入 argv，并在 transient systemd unit 结束后清理。

## State 与生命周期

State 目录包含 `oci/blobs/sha256`、`runlab.sqlite3` 和 Linux 执行时使用的 `engine` 目录。`engine` 内的 `invocations` 只保存调用期间的私有 workspace，正常返回后为空；`snapshots-v3` 保存由有序 DiffID chain 标识的只读 OverlayFS snapshot、初始 filesystem Inventory 和 Layer 验证收据。不可变的 Local OCI Store 命中精确 Descriptor、DiffID 与 uncompressed-size 收据后复用验证结果；可变 Store 仍逐次复验。每次调用使用独立 upperdir/workdir，Final Image 捕获只读取本次 upperdir。OCI 内容按 Descriptor 的 size 与 digest 校验，并通过同目录临时文件原子发布。Image 名称只在完整 Manifest、Config、Layers 和 DiffIDs 验证后写入 Catalog。

State 的普通命令持有共享 maintenance lease；`storage prune apply` 必须取得非阻塞独占 lease，因此不会与 Run 或其他 State 操作并发删除内容。`storage status` 分开报告文件系统容量、数据库、OCI、snapshot cache、invocation staging、其他 State 占用、资产引用和缺失引用。`storage prune check/apply` 只处理未引用 OCI blob、不可达 snapshot chain 和遗留 invocation staging；Catalog、Run Record 及其可达 OCI graph 始终保留。

Run identity 由调用者提供 canonical lowercase UUID v4。`run start` 在调用 Engine 前写入 accepted record；同一 identity、语义相同的输入与相同 metadata 返回已有记录，输入或 metadata 不同则拒绝。Engine 正常返回 `RunOutput` 或 `EngineError` 后写入 terminal completion。stdout 只返回有界摘要，包括 metadata、lifecycle、execution、各 Program 的 process、final environment 与 errors；stderr 同时输出以 `run.stream` 开始的实时 NDJSON，包含 `run.stage`、`program.stdout` 与 `program.stderr`。完整输入和持久标准流仍通过显式 `run get` 读取。

`run start --detach` 启动同一 Coordinator 的独立进程组，只等待 accepted 事实可见后返回 Run ID 与恢复命令。它不建立第二种 Run，也不改变 Run Protocol；worker 继续走相同 accepted→Engine→terminal 生命周期。detached 调用不转发 Program stream，后续只通过 `run get` 或 `query run` 观察。

`exec` 使用同一 Request Builder 与 `NativeEngine`，但把 `RunControls.capture_final_environment` 设为 `false`。它没有 `run_id`、metadata、accepted/terminal record、Query/get 面或恢复语义，也不发布 OCI Final Image。stderr 观察流的头部为 `run_id: null`，且不会发出 `accepted`、`capturing`、`publishing` 或 `terminal` 阶段；stdout 直接返回完整的有界 `RunOutput` 或 `EngineError`，Program 的 Final Environment 明确为 `not_requested`。`exec` 会真正运行 Program 并保留外部副作用，适用于 `run start` 前的检查和 Observation，不是模拟执行。

当前不实现跨进程恢复。进程在 accepted 之后、terminal 写入之前崩溃时，记录会诚实地保持 accepted 状态；没有 reconcile 或隐式重试。

## 已知边界

- `NativeEngine` 只在 Linux 可执行；macOS 通过 Managed VM 使用同一个 Linux binary 与 Engine，不存在 macOS Engine 实现。
- `NativeEngine` 支持 `Network::Isolated` 与 outbound-only `Network::Egress`。Egress 依赖宿主启用 IPv4 forwarding，并提供 `ip`、`iptables`、`ip6tables` 与 `nsenter`；Engine 不修改宿主级 forwarding 设置。
- Image import 只接受包含单个 Image Manifest 的标准 OCI Image Layout 目录或未压缩 tar archive。
- 支持 OCI tar、gzip 和 zstd Layer；不实现 registry pull 或 Image build。
- NativeEngine snapshot cache 没有独立的容量策略；不可达 chain 由 `storage prune` 的只读计划和显式 apply 回收。
- Native Linux 与 macOS Managed VM 的 `filesystem get` 都支持文件、目录和 symlink。
- `filesystem get` 从 Run Program 的 Final Environment 或指定 Image 读取普通文件、目录或 symlink。目标路径必须尚不存在；单文件从最新 Layer 向前解析，目录只合并目标子树。OCI hardlink 按其所在 Layer 的 filesystem 视图解析为普通文件内容，避免错误跟随后续 Layer 对 link target 的覆盖。
- `filesystem changes --run` 只派生 Final Environment 相对 Initial Image 的变化路径。它按路径稳定排序并分页，区分 `added`、`modified`、`deleted`；Final Layer 的候选路径只触发一次 Initial Layer chain 扫描，opaque whiteout 用 `subtree: true` 的目录事实表示，不展开全部下层后代。
- stdout/stderr 既作为协议事实保存在完整 Run record 中，也在 `run start` 期间作为实时观察事件输出；独立 stream 命令尚未因真实场景而引入。
- `exec` 的 stdout/stderr 不持久化，只在最终 stdout JSON 与实时 stderr 观察流中返回；调用方丢失结果后没有按身份恢复的读取面。
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

`docs list/get` 已通过跨平台独立进程测试：root/help 可以发现该命令；无效的 `RUNLAB_STATE` 与 `RUNLAB_LIMACTL` 不影响本地文档读取；Markdown 与 JSON 返回相同正文；未知 topic 以非零状态、空 stdout 和 `docs list` 提示失败。registry 单元测试另外覆盖稳定名称与跨 checkout 的 CRLF 归一化。`cargo package --list` 已确认 Markdown source、registry、CLI adapter 和独立进程测试均进入 root package 文件集合。

Image 与 Run metadata 已在 Managed Linux VM 中通过独立进程 CLI 测试：`image import` 写入的 description 和任意字符串 labels 会由名称形式的 `image get` 与 `image list` 返回，digest 形式查询返回 `metadata: null`；`run get/list` 返回持久 metadata；旧版 Catalog 与 Run 表会迁移并为既有记录补充空 metadata。相同 `run_id` 的 metadata 相等判断、8 KiB 上限、重复 label key、包含 `=` 的 value、macOS 参数转发和完整 CLI help 也有可执行覆盖。

Run Query Plane 已在真实 Managed VM State 上验证：Run `fae36f03-6c87-4298-aac9-87121ad209a5` 在 accepted 时保存 `initial_image_name: "base"` 与 label `validation=query-plane`，后续 query 可按这两个事实选中该 Run，并返回 terminal、exited 和 exit code 0。旧 Run 的 Initial Image name 为 `null`，迁移不会从当前可变 Catalog 倒推历史事实。读取 `main.runs`、`sqlite_schema` 与执行 `DELETE` 均以非零状态被拒绝；`--limit 1` 显式返回 `complete: false` 和 `incomplete_reason: "row_limit"`。macOS 只将经解析的 query 参数与经 size/digest 复验的 SQL 文件传入 Guest，没有任意 argv 通道。

当前 Linux release binary digest 为 `sha256:61a70ab2bc08e74ba6bdfb03e0bd049025f369e81cdec9b8ff6e41f71fc90548`，已安装到 Managed VM 并通过真实 `base` Image 与 `runc` 验证 `exec`。确定性 Program 分别写入 `exec-stdout`、`exec-stderr` 和私有 rootfs 文件后 exit 0；stdout 返回完整的两路 Base64 bytes，Final Environment 为 `not_requested`，stderr 流头为 `run_id: null`，阶段只有 `preparing` 与 `executing`。一次无并发写入的前后比较中，Run 总数保持 49，OCI blob 文件集合逐项保持 235 个，invocation workspace 为空，证明该调用既未建立 Run，也未发布 Final Image。第一次 blob 计数观测为 232→235，但同期早先接受的 Run `e5d5cdfd-90f2-4efa-b182-fef4fe6adca6` 恰在比较窗口内 terminal 并发布三个 OCI 对象，因此该次比较受并发写入污染，不作为 `exec` 结论。

`exec` 中断也由前台 systemd unit 独立验证：在 Program 输出 `interrupt-started` 后向 Guest CLI MainPID 发送 SIGINT，最终 `cancelled: true`、`timed_out: false`，Program 在共享停止宽限期后以 signal 9 结束，观察流出现 `stopping` 而没有 `capturing`，调用后 workspace 为空。更早一次把 CLI 作为非交互 shell 后台 job 的探针没有触发取消；后台 job 的 SIGINT 处置改变了测量路径，因此该结果只证明探针无效，不用于评价 CLI 中断行为。

真实 Linux CLI 纵切已通过 `runc 1.5.1`：导入 arm64 OCI Image，执行返回 exit 7 的 Run，保存独立 stdout/stderr 与 Final Image，通过 Final Image digest 提取 `/result/value` 的精确字节，并验证同 identity 重试返回 `created: false`。单独的长运行进程收到 SIGINT 后得到 terminal、`cancelled: true` 的 RunOutput，Engine workspace 无残留。实时观察纵切中，`run.stream`、`accepted`、`preparing` 在 0.107 秒到达，`executing` 在 0.242 秒到达，Program 首批 stdout/stderr 在 0.253 秒到达，Program 两秒后的第二段 stdout 在 2.251 秒到达，最终 stdout 摘要在 2.621 秒返回，证明事件没有等 Run 完成后批量输出。

紧凑 `run start` 结果也已通过真实 Linux CLI 验证：确定性 Program 分别写入 `compact-stdout` 与 `compact-stderr` 后 exit 7，命令返回 663-byte 摘要，保留 process 与 Final Environment 且不包含两个 stream payload；随后 `run get` 从完整 Record 精确恢复两段字节。同 identity 重试只把 `created` 改为 `false`，其余摘要一致。

真实 `NativeEngine` E2E 还覆盖了 `Network::Egress`：Program 从独立 OCI network namespace 主动连接 VM 上的 TCP 服务并取得响应；调用返回后临时 veth 与对应 IPv4/IPv6 firewall rules 均无残留。

Egress 的宿主网络变更由 `/run/run-engine-network.lock` 在所有 NativeEngine 进程之间协调；锁只覆盖 setup/cleanup，不串行化 Program 执行。Program 的 host-wide proc connector 订阅在网络准备结束后、OCI start 前建立，避免把 `ip`、`iptables` 和 `nsenter` 辅助进程事件积压成当前 Program 的结果监控失败。真实 Linux 验证同时覆盖同一 NativeEngine 的两个 Egress 调用，以及共享同一 VM 和 State 的两个独立 CLI 进程：两条确定性 HTTP Run 与两条 DNS+HTTPS `exec` 均得到精确 exit 0，Program 和 execution errors 为空，未出现 `ENOBUFS`，结束后 veth 和 IPv4/IPv6 规则均无残留。

detached 纵切以真实 Managed VM 和 `base` Image 验证：Host 在 accepted 可查询后返回，不等待 Program terminal；同一 Run 随后由 `run get` 取得 terminal。引入不可变 Store 的 snapshot validation receipt 后，11-Layer warm Run 的 accepted→Program start 从此前 31.904 秒降为 0.775 秒，Program end→terminal 为 0.144 秒；这是同一 VM 上的单次前后观测，不表示统计分布。`image get base` 也只读取 Manifest 与 Config，真实 macOS 调用 wall time 为 0.838 秒，不再为只读 metadata 重验所有 Layer。

`storage status` 在真实 36,323,454,976-byte VM 文件系统中报告 State 8,725,782,528 bytes，其中 OCI 2,562,068,480、snapshot cache 6,017,396,736、database 7,507,968、other State 138,809,344；256 个被引用 OCI blob 没有缺失。一次 `storage prune apply` 按 check 计划删除 3 个未引用 blob，共 27,521,024 bytes，没有删除任何 snapshot chain、Catalog、Run 或已引用内容；再次 status 的 reclaimable 全部为零。

`image export` 已通过 Linux 独立 State 的 Catalog→archive→第二 State round trip 和 Final Image 导出验证。真实 macOS transport 又把 `secret-e2e` 导出为 1,012,736-byte archive，并列出 `oci-layout`、`index.json`、Manifest、Config 和 Layer。第一次真实调用暴露 root-owned mode 0600 Guest archive 无法由 transport 用户校验；该结果失败且不作为成功证据。修正后 archive 只转交给当前 Guest 用户、保持 mode 0600，经 Guest/Host size 与 SHA-256 双重校验后原子发布，独立进程测试固定了这条权限边界。

Secret 纵切已从 macOS CLI 经 Managed VM 和真实 `runc` 验证：Program 同时读取一个 Secret 环境变量和一个只读 Secret 文件并退出 0；`run get` 只返回 `retained: false`，Final Environment 可以取得普通结果文件但无法取得 Secret file。NativeEngine opt-in real-runc 全生命周期测试也覆盖同样的 env/file 交付、Final Environment 排除和 workspace 清理。

Secret 环境变量还通过一次完整 macOS Agent User Story 验证：`pi + deepseek-v4-flash` 在 SWE-bench `psf/requests-5414` Image 中通过 `--secret-env DEEPSEEK_API_KEY` 完成任务，Program exit 0，无 execution/program error。`filesystem get --run` 取出的 571-byte `/artifacts/solution.patch` 与既有 golden patch 字节相同，SHA-256 均为 `adfa5771ae09b6ff1d91eb2a57943d20f0a899df777528a2233821e8f73fc20a`。

NativeEngine snapshot cache 与 upperdir-guided Final Image capture 已在同一 Managed VM 以 release binary 和 11-Layer SWE-bench Image 做 cold/warm 单次对照。snapshot chain 只发布 `upper`、`directories.bin` 和 `chain.bin`，解码后的 Layer 与 staged file 只存在于 build scratch；实际 cache 为 639 MiB，其中 Inventory 为 4.4 MiB，成功发布后没有 `build-*` 或 scratch 残留。cold `/bin/true` Run 从 accepted 到执行开始为 29.601 秒，warm Run 为 2.820 秒；对应完整命令 wall time 为 33.88 秒和 7.05 秒。进程结束到 terminal record 分别为 2.420 秒和 2.373 秒，两个 Run 均无 execution 或 Program error，Final Image digest 都是 `sha256:35f222e7175d8cc7bac5614f1fa0666d92ac7856d34d92c24859c11dc59dcd81`。另一次 warm Run 写入 5-byte `/artifacts/solution.patch`，wall time 为 7.13 秒，Final Image `sha256:9116f754d82d79aef469ba2f5cb4ed60afa0fd870591f2f1831c833c4fbf5f76` 可由 `filesystem get` 取回相同字节。调用后 invocation workspace 均为空。这些是固定环境中的各一次观测，不表示统计分布。

Runtime Configuration 生成能力已在同一 Linux VM 通过真实 CLI 纵切验证：`run config generate` 的精确 stdout 字节分别被省略 `--runtime-config` 的 `isolated` 和 `egress` Run 原样保存，两个 Run 都通过 `runc` 正常退出。生成的 JSON 包含新的 network namespace，但不包含 Run Protocol 的 `network` 字段。另一路径通过 `jq` 修改生成配置的 `process.args`，再以显式 `--runtime-config` 执行并取得预期 stdout。

Agent `base` Image 已由当前 `images/Dockerfile` 在 Docker Buildx 上构建成单 Manifest `linux/arm64` OCI archive，并导入 Managed VM Catalog 名称 `base`。Manifest digest 为 `sha256:7c5863478066a07c7222ef32bbfd6c4890a9dbf6ed7d84f3c9fee29543b6bfa6`，Config digest 为 `sha256:242d9bc64fafe946d7f6ce3b6ec07f83393fc1a30cd3c878804ba1861f824154`；7 个 Layer 共 312,484,616 compressed bytes、941,358,080 uncompressed bytes，其中最后一个是 32-byte canonical empty Layer。Catalog metadata、`run config generate` 得到的 uid/gid 1000、环境、`/workspace` cwd 和 `/bin/bash` args 均与构建契约一致。

base smoke 的 cold Run `336a31a2-8789-438d-968d-4e7b5fdfa12d` 与 warm Run `0287532e-581d-4e70-878d-1afc4ec31394` 均在真实 NativeEngine 中 exit 0，且无 execution 或 Program error。验收覆盖全部 51 个显式 apt package、常用命令、普通用户和目录权限、空 cache/credential/workspace，并观察到 Python 3.12.3、uv 0.12.7、Node.js v24.20.0、npm 11.19.0。cold accepted 到 executing 为 34.539 秒，warm 为 3.502 秒；完整 host CLI wall time 分别为 46.50 秒和 14.02 秒。`filesystem get` 从 cold Run Final Image 取回 257-byte `/artifacts/base-smoke.json`，内容 digest 为 `sha256:22c0e1afc7504284c889c04d1658f38f5a9a095cc203fb75cd5bd159749f2e4c`。

首次构建也如实暴露了上游身份事实：锁定的 Ubuntu 24.04 Image 已包含 `ubuntu:1000:1000`，直接创建 `agent` 会因 GID 冲突失败。最终 Dockerfile 原地将该用户和组重命名为 `agent` 并迁移 HOME，没有创建重复数字身份。

四个 Agent Catalog 名称现在解析到以下 `linux/arm64` Image。它们的前 7 个 Layer descriptor 与 base 完全相同；`all` 的前 8 个 descriptor 又与 Pi 完全相同：

| Name | Manifest digest | Config digest | Layers | Compressed bytes | Uncompressed bytes |
| --- | --- | --- | ---: | ---: | ---: |
| `pi` | `sha256:a7d295aa5102ece7325568beadb61804f60e8d31e77c5f826f8aa0c71166a7b0` | `sha256:892436b8c809e39031678ed61734f5f545e1db3d0f1d014918c017c93a5fd42b` | 8 | 339,468,684 | 1,063,075,328 |
| `claude` | `sha256:850e23f0fdf05e62a70c595e90d65ec9dd89d4022cfe4ab520b4035b002d713a` | `sha256:b1c7840ae9bff5a2677a1427fcbe528a0fb8b9c7512e0a5da57d5eeea2347aac` | 8 | 409,211,967 | 1,164,697,088 |
| `codex` | `sha256:4246fa269ec2a9acf1aec225456e89bafea06d1166c0bf62fe124cff56f7597f` | `sha256:58f5d48768dc63ad9ea7cba0d4fcda1f0be439f476e5e445dbe6b13f3b3703a8` | 9 | 435,665,897 | 1,232,515,584 |
| `all` | `sha256:9ff162485fe0905d5e2212b707258fcf844d282a1e9b3a8d2052a11af99b52ea` | `sha256:0308428d229741fb77f0534373022e870b2cee1d9b5fdd63617e67eae91e8811` | 11 | 559,376,712 | 1,577,566,720 |

Pi 0.84.3、Claude Code 2.1.250 与 Codex CLI 0.150.1 都安装在不可由 uid 1000 修改的 `/opt/agents/<agent>`。各 Image 预建对应的 uid/gid 1000、mode 0700 可写状态目录，但不包含登录态。Pi、Claude 和 Codex 默认分别执行 `pi --approve`、`claude --print --dangerously-skip-permissions` 和 `codex exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -`；`all` 默认执行 `/bin/bash`。Layer 审计没有发现 npm 下载 cache、Node compile cache、日志或 Codex `tmp/arg0`。

最终 digest 的离线 smoke 都在真实 Managed VM / NativeEngine 中 exit 0，execution 和 Program errors 均为空；每个产物又由 `filesystem get` 从 Final Environment 取回：

| Image | Run ID | Artifact SHA-256 |
| --- | --- | --- |
| `pi` | `8c2f1467-acae-45c2-930c-cee597636684` | `bffbb13e3e740da410341ec459305f5c48bbffa95234ae94361e446a0e7cff7e` |
| `claude` | `10f4a9d7-c449-43c2-b0e0-9f7829ff42e1` | `15d33607b61bcb6480ee241161119a6d584631ad8893e8e07a92f25502bd010d` |
| `codex` | `ed87181a-8e40-47e3-8eda-35ec2fcfed37` | `563be2fff0d684d93dfe3384f2ccd399ba665ed10cc714475b35ad54de61ad24` |
| `all` | `09b9e261-055f-4755-9f99-011ad7ace005` | `d267dc19acc977aac355c14af098c171aac5e2d7a9619e7f1a836d37e6e960bc` |

最终 Codex Image 还通过 Run `b713b2be-1ea5-4852-b9e7-9ba83e5b3969` 完成一次真实订阅认证与 egress 调用：只读 Secret file 把宿主 `auth.json` 交付到 `/home/agent/.codex/auth.json`，Codex stdout 为 `RUNLAB_CODEX_OK\n`，process exit 0，且无 execution 或 Program error。Run Record 只保留目标与 `retained: false`；`filesystem get` 从 Final Environment 读取该 Secret path 得到 `Filesystem path does not exist`。宿主 Claude 订阅态当前只存在 macOS Keychain，没有可移植的 env 或 file credential，因此 Claude Image 当前只完成离线验收，不把本机认证复制进 Image，也不声称在线验证通过。

构建与运行检查曾拒绝多个中间结果。最早两个 Pi Image 分别把 `/home/agent/.npm` 与 `/tmp/node-compile-cache` 带入 Layer；旧 Run `8e90044d-7cbf-414a-8e8f-feec7433da50`、`7153c6f5-2f87-48f1-a2c0-7c6032717146` 不作为最终证据。Run `a3b88ea8-35ca-4247-85a7-188227d53ae1` 还因 smoke 误用 `readlink -f` 而 exit 1。Codex Run `fd9a05dd-e713-4760-a8ed-378033722178` 证明只注入 `OPENAI_API_KEY` 不等价于 Codex CLI 登录；Run `90159d40-6c4a-46c4-b349-bcc831249d91` 暴露旧 Image 缺少可写 `/home/agent/.codex` 状态目录。一次 Layer 审计又发现 tmpfs 内创建的状态目录没有进入独立 Codex Image，最终实现把它拆成单独的 146-byte Layer。所有旧 Catalog digest 已被上表中的最终 Image 替换；旧 Run Record 均未删除或重写。

更早的 Pi Image `sha256:405d2c8d3bcca9816efb4901349f2bfe0a70b541689965f20732ff17951217ef` 曾由 Run `cfb833d3-896d-4738-8b52-336f7ee313c8` 通过 `deepseek/deepseek-v4-flash`、Secret env 与 egress 返回 `PI_OK\n`。之后只增加了空的可写 Pi 状态目录，形成当前 Manifest；由于没有重跑同一在线调用，该旧 Run 只作为直接前身的功能证据，不冒充当前 digest 的在线证据。

`filesystem get --run` 已在同一 Linux VM 对真实 SWE-bench Run 验证。命令从最终 Image 取出 571-byte `/artifacts/solution.patch`，内容 digest 为 `sha256:adfa5771ae09b6ff1d91eb2a57943d20f0a899df777528a2233821e8f73fc20a`。最终代码的 release 构建首次观测为 27 ms，随后六次为 17–20 ms；debug 构建随后六次约为 179–180 ms。此前正序读取并重复校验全部 Layer 的实现稳定约为 30.8–31.0 s。

`cargo package -p run_protocol --no-verify --locked --allow-dirty` 成功。完整 workspace packaging 当前不能成立：`run_engine` 打包时会从 crates.io 解析 `run_protocol 0.1.0`，而该版本尚未发布。没有为绕过这一发布顺序去修改 manifest 或评价路径。
