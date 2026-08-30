---
title: "RunLab Run Live Event 与 CLI 输出"
description: "定义 run start 与 exec 期间 stdout、stderr、Live Event 阶段、Program 标准流复用以及事实边界。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# RunLab Run Live Event 与 CLI 输出

RunLab 必须让长时间运行的协议调用可以通过 Live Event 被实时了解，同时不解释 Program 的领域行为。`runlab run start` 与 `runlab exec` 都保持同步调用；调用方等待最终结果期间，stdout 只承载最终命令结果，stderr 默认承载 Live Event 流。

本页定义 RunLab CLI 的输出通道与 Live Event 契约。[Run Protocol](/design/generated/run-protocol) 仍只定义 `RunInput → Result<RunOutput, EngineError>`，不会因为产品需要实时显示而增加持久身份、事件流或 Agent Trace 语义。Live Event 是执行期间的非持久进度旁路；Observation 专指 terminal Run 上追加的持久、typed、Method-attributed record，两者不能混用。

## stdout 与 stderr 的责任

| 通道 | 责任 |
| --- | --- |
| stdout | `run start` 正常返回时写入带 Run 身份和终态摘要的紧凑 JSON；`exec` 没有后续读取面，因此写入完整的有界 `RunOutput` 或 `EngineError` JSON。stdout 不混入进度。 |
| stderr | 输入解析完成、即将调用 Engine 时开始写入 NDJSON Live Event 流，包括阶段、Program 的 stdout 与 stderr，以及执行或传输诊断。 |

stderr 的每个物理行都是一个完整 JSON 对象。RunLab 不在这些对象之间插入装饰文字，也不把 Program 原始字节不加边界地直接混入 stderr。调用方可以忽略 stderr、把它重定向到文件或其他消费者，或者只选择自己关心的事件；RunLab 不通过 `--follow` 一类开关决定实时信息是否存在。

`kind` 采用 `<subject>.<event>` 命名。点号左侧表示记录所描述的语义主体，不表示发送者；这些记录的生产者始终是 RunLab。例如 `program.stdout` 描述 Program stdout，而不是要求 Program 自己实现事件协议。

请求在建立执行边界前失败时不会产生本页定义的 Live Event 流。命令失败仍通过非零退出状态和 CLI 诊断表达。

## Live Event 流头

每条新的 stderr Live Event 流以独立的 `run.stream` 记录开始。它只声明整条流不再变化的 `schema_version` 和可空 `run_id`：

```json
{"kind":"run.stream","schema_version":1,"run_id":"550e8400-e29b-41d4-a716-446655440000"}
```

`run start` 使用已经接受的持久身份；`exec` 没有持久身份，必须明确写出 JSON `null`：

```json
{"kind":"run.stream","schema_version":1,"run_id":null}
```

流头不表达 Run 生命周期事实。初次 `run start` 在流头之后发出真实的 `stage: "accepted"` 事件；`exec` 不发出 `accepted`。重新建立 Live Event 连接时只重新发出流头，不能因此伪造一次新的接受。

后续事件不重复 `schema_version` 和 `run_id`，只包含共同字段 `kind`、`observed_at` 以及该事件自己的字段。调用方必须从流头开始解释一条 Live Event 流。

NDJSON 中的行序表示 RunLab 发出 Live Event 的顺序，不证明不同进程或不同标准流在内核中的原始写入顺序。`observed_at` 也不能替代 Run Protocol 在最终输出中保存的执行时间事实。

## Run 阶段事件

`run.stage` 只在 RunLab 直接进入一个阶段时发出，不给出百分比、剩余时间或“任务是否成功”的判断。

```json
{"kind":"run.stage","observed_at":"2026-08-28T03:10:01Z","stage":"executing"}
```

稳定阶段及其含义如下：

| `stage` | 已经发生的边界 |
| --- | --- |
| `accepted` | Run 身份和脱敏输入已经持久化。 |
| `preparing` | Engine 正在验证输入并准备执行资源，尚不能声称 Program 已启动。 |
| `executing` | Run Protocol 执行区间已经进入。 |
| `stopping` | 超时、取消或协调结果使执行进入有界停止流程；正常退出可以不经过此阶段。 |
| `capturing` | 相关进程已经不能继续写入，Engine 正在取得最终环境。 |
| `publishing` | Engine 已经返回，Coordinator 正在发布持久 completion。 |
| `terminal` | 持久 completion 已经发布，可以通过 Run 读取操作取得。 |

`accepted`、`publishing` 和 `terminal` 只适用于 `run start`。`capturing` 只在 `RunControls.capture_final_environment` 为真且 Engine 实际进入捕获时出现；当前 `exec` 明确关闭捕获，因此不会出现该阶段。`preparing`、`executing` 与需要时的 `stopping` 适用于两种调用。

失败可能使某些阶段没有进入，因此消费者不能要求每次调用都出现完整阶段序列。阶段事件只说明已经观察到的边界，不能根据后续事件补发一个实际上没有观察到的早期阶段。

## Program 标准流事件

`program.stdout` 与 `program.stderr` 把 Engine 已经从 Program 对应管道排空的字节实时复用到 CLI stderr。事件明确携带 `program_id` 和该流内的 `byte_offset`；`kind` 已经区分 stdout 与 stderr，不再增加重复的 `stream` 字段。

```json
{"kind":"program.stdout","observed_at":"2026-08-28T03:10:02Z","program_id":"primary","byte_offset":0,"text":"starting agent\n"}
{"kind":"program.stderr","observed_at":"2026-08-28T03:10:03Z","program_id":"primary","byte_offset":0,"base64":"AP8="}
```

每条事件有且只有 `text` 或 `base64`。能够完整表示为 UTF-8 的片段使用 `text`；不能无损表示的片段使用标准 Base64 编码的 `base64`。RunLab 在读取边界恰好落入 UTF-8 字符内部时保留不完整尾部，等待后续字节后再决定使用 `text`，不会仅因一次管道读取的任意分块而降级成 Base64。按同一 `program_id` 和 `kind` 的 `byte_offset` 顺序解码并连接，可以还原已经送达调用方的原始字节。分块边界没有 Program 语义。

只要实时连接可用且消费者能够跟上，RunLab 就转发 Engine 排空的全部字节，包括超过 Run Protocol 单流 100 MiB 保留上限后继续排空但不进入 `RunOutput` 的字节。Live Event 记录经过有界队列，Engine 与调用层不等待 stderr 消费者；连接中断或消费者持续落后时可以丢弃尚未送达的 Live Event，并在通道仍可写时发出 `transport.diagnostic`。`byte_offset` 的不连续使 Program 流缺口可被发现。无论 Live Event 是否完整，标准流排空、执行与最终事实收集都必须继续；`run start` 的持久发布也必须继续，不能因为 Live Event 消费者而改变 Program 行为。

Program 输出只作为 `program.stdout` 或 `program.stderr` 的数据出现，不能伪造 `run.stage` 或诊断事件。Run Protocol 仍分别保存 stdout 和 stderr，并且不从实时发出顺序推断两条流之间的原始顺序。

## 诊断事件

RunLab 在执行尚未终结但已经观察到一个值得调用方注意的执行条件时发出 `run.diagnostic`。它用于报告 Coordinator 或 Engine 中会影响当前调查的事实；正常成功路径不输出底层 Runtime 的内部调试噪声。

诊断至少给出 `message`，能够定位到具体操作时给出 `operation`，能够归属于 Program 时同时给出 `program_id`。Live Event 通道自身的问题使用独立的 `transport.diagnostic`，防止 Managed VM 连接问题被误解成 Run 执行结果。

```json
{"kind":"run.diagnostic","observed_at":"2026-08-28T03:10:04Z","operation":"final_environment_capture","program_id":"primary","message":"final environment capture failed"}
{"kind":"transport.diagnostic","observed_at":"2026-08-28T03:10:05Z","operation":"managed_vm_forward","message":"managed VM observation connection ended unexpectedly"}
```

Live Event 诊断不是新的 Run 事实权威。Engine 正常返回后，`RunOutput` 拥有执行和 Program 操作错误；Coordinator 发布后，Run Record 拥有持久 completion；传输诊断只说明调用方 Live Event 通道发生了什么。消费者不能仅凭某条诊断事件重建终态。

## 不解释 Program 语义

stderr 不包含 RunLab 推断的 Agent“思考中”“调用工具中”或“任务完成”等阶段，也不包含分数、排名、完成百分比和预计剩余时间。Program 如果主动把自己的 Trace、工具调用或进度写入标准流，RunLab只把它作为原始 Program 输出转发和保存。

RunLab 不扫描 Program 输出以猜测其中是否含有 Secret。Program 主动写入标准流的 Secret 按普通输出处理，并会进入 Live Event 流；Secret 交付本身的值、宿主来源和 Engine 私有临时路径不能由 RunLab 诊断主动输出。完整 Secret 边界由 [RunLab 系统设计](/design/generated/system-design) 负责。

## macOS Managed Linux VM

Linux 虚拟机中的同版本 RunLab 为 `run start` 和 `exec` 产生与原生 Linux 相同的 stderr NDJSON。macOS CLI 必须在事件到达时立即转发，不能等 Guest 命令结束后一次性返回，也不能解析后重写 Program 数据。Host 自己发现的连接或传输问题使用 `transport.diagnostic` 表达。

`run start` 的 Live Event 连接意外断开不表示取消 Run。虚拟机中的 Coordinator 继续持有执行，调用方随后依据同一个 `run_id` 读取持久结果，或通过 `run cancel` 显式提交取消请求。当前前台 CLI 收到 `SIGINT` 或 `SIGTERM` 则是显式取消：Host 必须把请求送到对应 Guest 调用并继续等待有界终结结果。`exec` 使用相同的前台信号语义，但没有持久身份或后续读取面；连接在没有显式信号时中断，不能把结果恢复为 Run，也不能声称执行未发生。这项所有权边界由 [macOS Managed Linux VM](/design/generated/macos-managed-vm) 定义。
