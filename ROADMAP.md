# Run Protocol 与 Run Engine 实施路线

本文只记录当前 worktree 的实施顺序、Review Gate 和尚未闭合的验证事实。稳定产品、协议和架构由 [Agent Wiki](http://localhost:8787/app/pages/runlab-index--nw) 拥有；当前已实现范围见 [IMPLEMENTATION.md](IMPLEMENTATION.md)。

## 已固定边界

Cargo workspace 最终只有三个 package：

```text
runlab → run_engine → run_protocol
   └────────────────→ run_protocol
```

- `run_protocol` 只拥有 `RunInput`、`RunOutput`、`EngineError` 和协议不变量。
- `run_engine` 拥有 `RunEngine`、调用级取消、OCI 内容访问、`NativeEngine` 和 `DockerEngine`。
- `runlab` 拥有 CLI、请求构造、Catalog、Run identity、Run Record、Storage、Coordinator 和 VM transport。

当前阶段只完成前两个 library package。`runlab` 的产品接线与旧执行路径删除属于下一阶段，不在协议或 Engine API 中预留兼容模型。

明确不实现：

- `check()` 或 `ValidInput`；
- `run_id`、Accepted/Terminal Record 或 Run Asset；
- `protocol_version` 或 `V1` 类型；
- Engine recovery、journal 或 reconcile；
- VM Engine、额外 Backend trait、async runtime 或 SDK wrapper。

## Gate 0：迁移前 checkpoint

迁移前多线 worktree 已保存为 checkpoint。普通 Rust 门禁和独立 review 必须绑定该提交；未运行的真实 Docker、Linux Native、rootless 和 runtime probe 保持明确未验证。

Exit gate：checkpoint commit、完整文件清单、测试命令和环境缺口均可复查。

## Gate 1：架构权威

- Agent Wiki 软件架构明确三个 package 与单向依赖。
- Run Engine 实现契约明确 `NativeEngine` 和 `DockerEngine`。
- 正式 Run Protocol 页面保持机制中立。
- `AGENTS.md` 与 Wiki 一致。
- Engine 实现文档不包含持久身份或恢复接口。

Exit gate：文档独立 review 通过后才创建新 crate。

## Gate 2：`run_protocol`

实施顺序：

1. OCI Descriptor、Digest 与 exact Runtime Configuration。
2. `RunInput`、Program、stdin、timeout 和 network 不变量。
3. `RunOutput` 的 execution 与逐 Program 事实。
4. `EngineError` 与 workload 结果的严格边界。
5. 协议 conformance、负向词汇和依赖审计。

API review 必须确认：

- Runtime Configuration 保留原始字节，typed view 不替代内容身份；
- `primary` 唯一且 Program 映射完整；
- 未尝试、失败、未知和不可取得分别表达；
- 非零退出、signal、timeout、取消和 create/start failure 不被误分类为 `EngineError`；
- 首版不意外冻结未经设计的 JSON wire format。

Exit gate：crate 可独立完成 fmt、all-target check/test、Clippy、MSRV 和 package，且独立 reviewer 通过。

## Gate 3：`run_engine` 公共边界

实现同步、阻塞且可并发复用的 `RunEngine`，以及调用级 `CancellationToken` 和范围窄的 `OciContentStore`。

Exit gate：接口不依赖 `runlab`，没有产品身份、存储或恢复概念；取消状态与每次调用隔离；未增加额外 Backend trait；独立 reviewer 通过。

## Gate 4：OCI Image 执行管线

```text
Descriptor
→ exact-byte verify
→ Manifest / Config / Layers
→ private rootfs
→ stopped filesystem capture
→ deterministic Final Image
→ content-addressed publish
```

Exit gate：digest、size、media type、Layer 顺序、filesystem 语义和 Manifest-last publish 均有测试证据。

## Gate 5：两个真实 Engine

公共边界冻结后，Docker 与 Native 可以在互斥目录中并行实施。

### `DockerEngine`

- 只接受能够逐字段忠实映射的 OCI 子集；
- 所有 unsupported input 在 Program 启动前拒绝；
- Docker CLI 状态不冒充 Program 事实；
- 支持标准流、timeout、取消、有界停止、最终环境和调用内 cleanup；
- 只连接本机 Docker Engine 或本机 Docker Desktop VM。

### `NativeEngine`

- Linux-only，Rootful 是 reference profile；
- rootless 是同一个 Engine 的受限 profile；
- 实施 OCI create/start/process、标准流、cgroup、network、mount、timeout、取消、最终环境和调用内 cleanup；
- 多 Program 不包含 readiness，所有 Program 拥有独立 rootfs 与结果。

Exit gate：Docker 在真实本地 Engine 验证；Native 在真实 Linux OCI Runtime 验证；每条路径分别独立 review 通过。

## Gate 6：跨 Engine conformance

共同 profile 覆盖 exit 0、非零退出、signal、create/start failure、stdin、stdout、stderr、固定 100 MiB 保留与继续排空、timeout、取消、10 秒停止宽限期、final environment、cleanup error 和并发隔离。

能力差异只能表现为“忠实执行”或“启动前准确拒绝”。测试断言协议事实，不比较实现私有标识、时间戳或底层错误文本。

只有两个实现都跑通后，才允许提取已经证明重复的私有机制；不增加公共 Backend trait。

Exit gate：全 workspace 门禁、真实环境测试、跨 Engine conformance 和最终独立 review 全部通过。

## 提交原则

checkpoint 之后恢复小而完整的 feature 提交。每个提交必须自行编译并携带对应测试。公共 API、Cargo workspace 文件和共享 conformance harness 由一个 integrator 持有；并行任务只能修改预先分配的互斥文件。

任何公共类型、错误分类、协议常量、依赖方向或新 trait 的变化都必须重新进入相应 Review Gate，不能在具体 Engine 实现中顺手修改。
