---
title: "RunLab 系统设计"
description: "定义单机 State、组件责任、Run Protocol 调用、持久化数据流和恢复边界。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# RunLab 系统设计

RunLab 的核心是一个单机可信数据面。标准 OCI 内容、持久 Run 事实和执行资源由明确的责任方管理，不依赖分布式控制面。

## State Directory

一个 State Directory 包含四类持久状态：

```text
state/
├── OCI Image Store
├── Local Image Catalog
├── Run Database
└── recovery / maintenance state
```

OCI Image Store 按 `digest` 保存确切内容字节。Catalog 保存可变名称映射及其本地 metadata。Run Database 保存 `run_id`、调用方提供的 Run metadata、接受时使用的 Initial Image Catalog 名称、`RunInput`、执行状态、结果，以及 terminal 后追加的 Observation 历史。Catalog 名称是可空的接受时产品事实：调用方直接使用 digest 或旧记录没有保存该事实时为 `null`，不能从之后可能已经变化的 Catalog 反推。恢复状态保存尚未闭合的执行资源所有权与协调证据。

缓存、临时 `bundle`、`container` 和物化 `rootfs` 是可重建或可清理的执行资源，不是新的事实权威。

## 组件责任

| 组件 | 责任 |
| --- | --- |
| Request Builder | 解析名称、产品默认值和 Secret 来源，构造确定的 `RunInput`。 |
| Storage | 用事务持久化 Run、Observation、Catalog、内容引用和恢复证据。 |
| Coordinator | 对 `run start` 验证调用方生成的 `run_id`，建立持久边界，调用 Run Engine 并发布结果。 |
| Run Engine | 实施 `RunInput → Result<RunOutput, EngineError>`。 |
| Image | 验证、展开、构造和发布 OCI Image。 |
| Query Plane | 通过稳定公共 Relation 提供有边界的只读 Run 与 Observation 选择和聚合，不暴露私有存储 schema。 |
| Maintenance | 验证 State、回收不可达内容并处理孤儿资源。 |
| CLI | 提供原子、可发现的操作，不拥有生命周期语义；按照 [RunLab Run Live Event 与 CLI 输出](/design/generated/live-events) 分离最终结果和 Live Event。 |

同一事实只能有一个 `owner`。Storage 不解释退出码，Run Engine 不发布持久 Run，CLI 不通过读取命令隐式修复状态。

## 持久 Run 数据流

```text
RunLab Request
（run_id + metadata + selectors + runtime config + secrets + controls）
                    ↓
          resolve / verify / construct
                    ↓
                 RunInput
                    ↓
   persist identity, accepted selector facts, metadata and redacted input
                    ↓
             RunEngine::run
               ├── 返回 Result
               │        ↓
               │  publish engine_returned
               │
               └── 调用或宿主机中断
                        ↓
                 reconcile evidence
                        ↓
                  publish interrupted
```

请求中的名称和默认值必须在调用引擎前解析。持久边界使 RunLab 即使在执行或结果发布期间中断，也能知道本次执行的身份、输入和资源所有权。Run Engine 正常返回时发布 `engine_returned`。恢复已经证明原调用不可能再返回时，可以发布 `interrupted`。两者是 RunLab 的持久 `completion`，不改变 Run Protocol 的返回类型。

执行期间，Coordinator 可以把已经观察到的阶段和 Engine 正在排空的 Program 标准流作为非持久 Live Event 旁路交给调用方。Live Event 不能修改执行、替代 `RunOutput` 或成为 Run Record 的第二事实来源；具体 CLI 通道和事件边界由 [RunLab Run Live Event 与 CLI 输出](/design/generated/live-events) 定义。

前台等待与 detached 提交是同一个持久 Run 的两种调用方式，不是两类 Run。前台调用持续连接 Coordinator 并接收 Live Event；detached 提交只等 accepted 事实可见便返回 `run_id` 和恢复读取入口，由独立 Coordinator 继续同一条接受、执行和终态发布路径。detached 调用不拥有之后的 Program stream，调用方通过 `run get` 或 Query Plane 继续读取持久事实。

持久 Run 通过 `runlab run cancel RUN_ID` 接受显式取消请求。Storage 在回应前原子保存请求，持有执行的 Coordinator 再把它交给本次 `RunEngine::run` 的 `CancellationToken`。重复请求幂等；Run 已经终态时只返回当前终态而不改写事实。请求被接受只证明取消意图已经持久化，不证明 Program 已经停止；实际停止动作和最终 `cancelled` 事实仍由 Engine 返回值拥有。

## 非持久执行数据流

`runlab exec` 复用相同的请求构造和 `RunEngine`，但不经过持久 Run 边界：

```text
Exec Request
（selectors + runtime config + secrets + controls）
                    ↓
          resolve / verify / construct
                    ↓
 RunInput(capture_final_environment = false)
                    ↓
             RunEngine::run
                    ↓
       return complete bounded Result
```

`exec` 没有 `run_id`、Run metadata、accepted/terminal Record、Query/get 面或恢复语义，也不捕获和发布 Final Image。它仍然真实执行 Program，实施 timeout 与 network，并保留外部副作用；适用于持久 Run 前的环境和命令检查，但不是 dry run，也不能成为 Observation 的主体。因为结果之后无法按身份读取，命令成功返回时 stdout 直接给出完整的有界 `RunOutput` 或 `EngineError`，而不是 `run start` 的持久摘要。

`exec` 的生命周期与当前同步调用相连，不为内部容器、进程或 Managed VM unit 暴露临时产品身份。调用方送达当前 CLI 的 `SIGINT` 或 `SIGTERM` 是对这次执行的显式取消；RunLab 必须把请求交给对应的 Engine 调用并继续等待有界终结结果。调用进程被强制终止或连接意外丢失不等于一个已经送达的取消请求，也不能在之后按身份恢复结果。

`exec` 不改变 Run Protocol 或 Run Engine 的抽象。是否捕获最终环境由协议中的 `RunControls` 明确表达，产品是否建立持久身份由调用路径决定；两者不能靠 Engine 猜测，也不能由 CLI 在执行后补写成 Run。

## Secret 边界

RunLab 在构造 `RunInput` 前从调用方明确选择的来源读取 Secret。CLI 的 `--secret-env NAME` 读取当前进程环境，`--secret-file HOST_FILE=CONTAINER_PATH` 读取宿主常规文件；宿主来源名称和路径不进入 Run Protocol。

作为 Secret 交付材料的精确字节只存在于本次调用的内存和 Engine-owned 临时资源中。Run Database 的公开 Run Record 只保存环境变量名、文件目标和 `retained: false`，不从 Secret 字段保存值。为了判定同一 `run_id` 是否绑定到相同输入，Storage 可以保存不对外返回的 Secret 内容摘要；它不是可恢复的 Secret 副本。Program 如果主动把 Secret 写入标准流或可写文件系统，这些字节仍会按普通执行结果保存；RunLab 不对任意 Program 输出做 Secret 猜测或内容扫描。

macOS 调用层先把 Secret 精确字节暂存到 Managed Linux VM 的私有输入文件，再由 VM 内同版本 RunLab 构造相同的 `RunInput`。暂存文件、Engine 派生的 bundle 配置和 Secret mount 都属于单次调用资源，必须在调用结束后清理。调用方原始 Runtime Configuration 仍按原始字节进入 Run Record；Engine 派生配置不进入持久输入。

## 内容可用性与事务

数据库事务可以原子保存 Descriptor 和状态，但不能让 OCI 内容自动存在。发布新内容时，必须先逐字节验证并按 `digest` 写入 Store，再让数据库引用它。删除内容前，必须确认所有持久引用和正在执行的引用都已排除。

`run_id` 标识 RunLab 的一次持久执行。环境 Descriptor 的 `digest` 标识 OCI 内容。内容是否仍可读取，需要由 Store 校验和保留策略另外证明。

Catalog Image 和 Final Environment 都能导出为标准 OCI Image Layout 或 archive。导出只是对已有内容的可移植表示，不让 RunLab 承担 Image 构建，也不引入另一套内容身份。

## Run 查询边界

RunLab 为不同读取目的提供三个层次：`run list` 只给出较小的最近 Run 摘要；公共 SQL Query Plane 负责按 Initial Image、metadata、时间、生命周期和终态结果做选择或聚合；`run get` 返回一个 Run 的完整持久事实。不能为了查询便利不断给 `run list` 增加专用筛选 DSL。

公共 schema 必须可以由同版本 CLI 发现。稳定 Relation `runs` 投影接受时的调用方事实、Initial Image digest、生命周期和少量终态结果；`observation_types` 投影不可变 Type 定义，`observations` 与 `observation_retractions` 投影通用 JSON payload、Observation 历史和派生的 active/superseded/retracted 状态；`run_deletions` 投影永久 tombstone 的 `run_id`、`deleted_at` 和 `operation_id`。内置与外部 Type 共用 Registry、validator、存储和查询路径，Type-specific 字段通过 SQLite JSON 函数选择，不设置专用列或 Relation。完整输入、标准流、错误和 Final Environment 仍由 `run get` 拥有。[RunLab Run Observation](/design/generated/observations)负责 Observation 的 Type、Method 和修正语义。公共 Relation 是产品契约，SQLite 私有表和内部 JSON 布局不是。

`accepted_at` 与 `terminal_at` 是精确 RFC 3339 文本事实，不能用字符串字典序做时间范围选择。`runs.terminal_unix_seconds` 是 SQLite 解析并舍入到毫秒的 Unix seconds，只用于范围筛选；审查与删除计划继续回显精确的 `terminal_at`。

一条查询只允许执行一个只读 SQL statement，并同时受行数、单 cell 字节数、总输出字节数和执行时间限制。返回值必须明确说明结果是否完整、因为哪种输出边界停止，以及有多少 cell 被截断。查询不能同步、恢复、验证、执行或修改 Run，也不能读取其他数据库或加载扩展。

## 并发、恢复与维护

同一个 `run_id` 的并发创建由 Storage 原子判定为新建、幂等重试或身份冲突。Coordinator 在调用引擎前保存恢复所需的资源所有权证据，并在得到结果后原子发布。

普通读取不改变 State。Storage prune 必须先报告文件系统容量、各类 State 占用、持久引用、引用图完整性和可安全回收内容，再由显式 `check`/`apply` 完成回收。Prune Apply 与所有普通 State 操作互斥，只能删除未引用的 OCI 内容、不可达执行缓存和遗留 staging，不能删除 Catalog 或 Run Record。恢复采用显式 `reconcile`；回收、删除和恢复都不能借只读查询悄悄改变记录。

Run Asset 删除只修改 SQLite，因此使用普通 shared State lease 和短 `BEGIN IMMEDIATE` 事务，不等待长时间执行的 Run 释放 shared lease。事务内重新读取所有 candidate，验证每条 Run Record 与 Observation 历史的共同 asset fingerprint，删除 `run_executions`、Observation 历史与 `runs`，写入永久 tombstone，然后整体提交。SQLite writer 超过 busy timeout 时返回 retryable Conflict，而不是暴露 `database is locked`。

## Run Asset 删除

“过期”是调用方的保留判断，不是 Run lifecycle。RunLab 不增加 TTL、后台 daemon、`expired` 状态、SQL 删除 DSL、`--force`、局部记录裁剪或自动 VACUUM。第一版删除完整终态 Run Asset；accepted Run 一律阻塞，并给出 `runlab run reconcile RUN_ID`。若 reconcile 的 durable evidence 仍为 `evidence_incomplete`，该 Run 继续作为 accepted 资产保留，本能力不提供绕过证据边界的处置路径。

调用方用公共 Query Plane 选择确切 ID，并提供自己拥有的 canonical UUID v4 `operation_id`。`run delete check` 把 ID 分成 candidate、非阻塞的 `already_deleted`、阻塞的 `not_found` 和 `not_terminal`。计划不包含 State 或计划文档的 aggregate digest：JSON 解析负责发现截断，每条 asset fingerprint 负责发现 Run Record 或 Observation 历史变化，文件系统预测不参与数据库计划的 stale 判定。

Apply 的 batch 全有或全无。同一计划提交后 stdout 丢失时，以相同 `operation_id` 重试返回 `already_applied`；同一 operation identity 不能绑定到不同 candidate 集合。tombstone 永久阻止已删除 `run_id` 被重新创建，并通过 `run_deletions` 供 Agent 批量发现。Schema v6 是有意的降级屏障：旧二进制不能打开已迁移 State，避免它忽略 tombstone、Observation Type Registry 或 Observation 资产。恢复旧版本需要恢复迁移前 State 备份。

Run 删除是持久存储生命周期操作，不是 secure erase。它先于实际 OCI/snapshot prune 独立提交，SQLite 文件也可能继续持有空闲 page；这些边界不能被描述成内容已经物理擦除。

## 单机与远端边界

State Directory 属于一台 Linux 数据面。macOS 可以通过受管理的本地 Linux VM 使用同一执行引擎。远端执行或分布式调度不能仅靠共享 State 路径实现，还需要另行定义通信、所有权和失败边界。
