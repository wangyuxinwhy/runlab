---
title: "Run 协调与持久化边界"
description: "定义 RunLab 在协议之外的接受边界、Engine 调用协调、结果原子发布和显式状态核验；不提供 Engine 调用中断后的跨进程恢复。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# Run 协调与持久化边界

本页定义 RunLab 如何在 Run Protocol 之外建立持久接受边界、协调一次仍然存活的 Run Engine 调用，并在 Coordinator 中断后核验已经持久化的状态。

Run Protocol 不定义 `run_id`、接受状态、数据库或状态核验。RunLab 增加这些产品机制，是为了让调用方能够在原进程消失后继续读取一条 Run 已经保存的输入、取消意图和完成结果。RunLab 不恢复、重启或接管一个没有返回的 `RunEngine::run` 调用。

## 持久 Run 的状态

公开生命周期只表达 RunLab 已经发布的持久事实：

```text
accepted → terminal
```

`accepted` 表示输入已经持久化、但尚未发布 completion。它不证明 Coordinator 或 Program 当前仍然存活。`terminal` 表示 RunLab 已经发布最终 completion。

私有 execution journal 可以记录更细的协调阶段：

```text
accepted → engine_running → result_staged → terminal
```

这些阶段属于 RunLab Storage，不是协议对象，也不是从进程存活状态推导出的公开生命周期。

稳定 completion 有两种形态：

```text
engine_returned(Result<RunOutput, EngineError>)
interrupted(RunInterruption)
```

只有 Run Engine 确实返回并且结果已经持久化时，才能发布 `engine_returned`。当前设计只在 journal 证明 Engine 调用从未开始、原 Coordinator 又已经死亡时发布 `interrupted`。`RunInterruption` 保存中断原因、观察时间、证据来源和无法取得的结果，不伪造成 `EngineError` 或部分 `RunOutput`。

## 接受边界

创建 Run 时，Coordinator 必须在启动 Engine 前原子保存：

- `run_id`。
- 可持久化的 `RunInput` 事实与私有 identity。
- 接受时间、metadata 和初始 journal 阶段。
- Coordinator 的 boot ID、PID 和进程 start ticks。

同一个 `run_id` 的请求由同一事务判断：不存在则创建，输入和 metadata 相同则返回现有 Run，不同则报告冲突。身份判断与记录创建不能分成可能竞态的两步。

Run Engine 仍会在 `run` 内验证协议输入和当前能力。RunLab 在接受前做的解析、内容验证或预检不能替代引擎的最终检查。

## 显式取消

`runlab run cancel RUN_ID` 表达调用方对一条持久 Run 的取消意图。它不是另一次 Run，也不修改原始 `RunInput`。Run 不存在时请求失败；Run 已经终态时返回当前终态且不改写记录；Run 尚未终态时，Storage 在回应前原子保存首次取消请求时间，重复请求返回同一事实。

仍然持有 Engine 调用的 Coordinator 观察已经保存的取消请求，并调用本次 `RunEngine::run` 的 `CancellationToken`。取消命令确认的是请求已经持久化，不是 Program 已经终止；最终是否观察到取消、实际发出的停止动作和对应结果只能来自 Engine 返回值。

如果 Coordinator 在 Engine 返回前消失，取消意图仍作为持久事实保留，但 RunLab 不启动新的 Coordinator、重新调用 Engine 或跨进程继续投递取消。`reconcile` 也不把取消请求推断成 Program 已停止。

## Engine 与 Coordinator 的责任边界

Engine 负责一次仍然存活的 `run` 调用内的进程监督、结果收集、Final Environment 捕获和调用内资源清理。`run_engine` 不接收 `run_id`，不访问 Run 数据库，也不定义 `RecoveryRecorder`、持久资源 checkpoint 或跨进程恢复接口。

Coordinator 只持久化产品层协调事实：owner identity、执行阶段、取消请求和 Engine 已经返回的完整结果。journal 不记录 Runtime 对象、进程、mount、网络、cgroup、workspace 或 Secret 临时文件的恢复身份。

因此，Coordinator 消失后，RunLab 不扫描、接管或清理一次 `engine_running` 调用留下的 Engine 资源。Storage 也不根据“owner 进程不存在”解释 Program 结果。

## 原子暂存与发布

Coordinator 在调用 Engine 前把 journal 从 `accepted` 推进到 `engine_running`。Engine 返回后，Coordinator 先把完整 `RunOutput` 或 `EngineError` 持久化为 `result_staged`，再以事务发布 Run completion、terminal 时间和 journal 终态。

这个顺序允许 `reconcile` 原样发布已经暂存但尚未公开的 Engine 结果。完全相同的结果可以幂等完成，不同结果不得覆盖已经发布的事实。

Engine 已经返回、但 Coordinator 在暂存结果之前死亡的窗口，与 Engine 调用过程中死亡在持久证据上不可区分。因为结果只存在于已消失进程的内存中，RunLab 不猜测返回值，也不把这条 Run 强行闭合。

## 显式 `reconcile`

`run reconcile RUN_ID` 是显式、可审计的持久状态核验。读取 Run、列出状态或检查内容不会隐式执行它。

`reconcile` 根据 Run Record、journal 和 owner identity 只执行下列动作：

| 持久证据 | 结果 |
| --- | --- |
| Run 已有 completion | 返回 `already_terminal`，不改写记录 |
| `result_staged` 保存了完整 Engine 结果 | 原样发布 completion |
| owner 仍由 boot ID、PID 和 start ticks 证明存活 | 返回 `coordinator_alive` |
| owner 已死，journal 仍为 `accepted` | 发布 `interrupted`，明确 Engine 从未调用 |
| owner 已死，journal 为 `engine_running` 且没有结果 | 保持 `accepted`，返回 `evidence_incomplete` |
| 旧 Run 没有 execution journal | 保持 `accepted`，返回 `evidence_incomplete` |

`reconcile` 不检查或清理 Runtime、进程、mount、网络、cgroup 和 workspace，不恢复标准流，不重新调用 Engine，也不把进程缺席推断为某种 `RunOutput`。

## 明确不提供的恢复能力

当前设计不提供：

- Engine 调用的跨进程恢复、继续执行或自动重试。
- Coordinator 消失后的 Program 接管、停止、收集或 Engine 资源清理。
- 持久资源 identity、资源 lease、恢复 checkpoint 或 `RecoveryRecorder`。
- 从取消请求、PID 缺席、Runtime 残留或部分 OCI 内容推导执行结果。

这些不是待实现的隐式能力。若未来要改变这一边界，必须先修改 [Run Engine 实现契约](/design/generated/engine-contract) 和软件架构责任，再设计相应的持久证据模型；不能由实现层自行增加恢复接口。

## 与 Run Asset 的关系

稳定 Run Asset 包含 Run Record 及其引用的 OCI Image 内容。execution journal、owner identity 和协调阶段是本地运维证据，不自动成为可移植资产的一部分。决定 terminal completion 的 interruption 证据会以稳定投影进入 Run Record。
