---
title: "RunLab 是什么"
description: "说明 RunLab 如何基于 OCI 与 Run Protocol 产生、持久化和管理可比较的 Run。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# RunLab 是什么

RunLab 是建立在 OCI 与 Run Protocol 之上的 Run 执行与资产管理系统。它将 Run Protocol 的一次执行持久化为一条有身份、可读取、可验证并能够用于比较的 Run。

[Agent Loop 实验的本质](/design/generated/agent-loop-experiment-model)把 Run 定义为一条执行输入、执行事实和执行结果都被明确记录，因而能够被理解和比较的程序执行。RunLab 是负责产生和保存这种 Run 的具体系统。

## RunLab 管理整个生命周期

RunLab 负责 Run 的整个生命周期：

1. 接收面向使用者的请求，并验证调用方生成的 `run_id`。
2. 解析 Image 名称、tag、默认值和其他便捷输入，构造确定的 `RunInput`。
3. 原子保存执行身份和输入，为中断恢复建立持久边界。
4. 调用 `RunEngine::run`，管理执行期间的协调和资源所有权。
5. 保存 `RunOutput`、`EngineError` 或没有正常返回时能够证明的中断事实。
6. 维护 Run Record、所引用的 OCI Image 内容及其完整性。

上述流程中，只有调用 Run Engine 的纯执行关系由 Run Protocol 定义：

```text
RunInput → RunEngine::run → Result<RunOutput, EngineError>
```

身份、接受、状态、幂等创建、持久化和恢复属于 RunLab，而不是 Run Protocol。

## 为什么使用 OCI

Run 需要把初始环境与执行配置明确分开，同时让环境拥有稳定、可传递的内容身份。OCI 已经提供了边界清楚的标准对象：OCI Image 描述受控文件系统，OCI Runtime Configuration 描述程序如何在该环境中启动。

| Run 概念 | 表达方式 |
| --- | --- |
| 初始环境 | OCI Image Descriptor 及其引用的 Image 内容 |
| 执行配置 | OCI Runtime Configuration |
| OCI 未覆盖的输入 | 有限标准输入、执行超时和网络控制 |
| 执行结果 | `RunOutput` 中的进程、标准流、停止动作和错误事实 |
| 最终环境 | OCI Image Descriptor，或明确的不可取得原因 |

RunLab 无需再发明一套环境和进程描述格式，也能让不同执行机制接收同一种协议输入。

## 受控环境与外部依赖

OCI Image 描述受控文件系统状态，OCI Runtime Configuration 可以显式引用 `bind mount`、已有 `namespace`、`hook` 和其他宿主资源。引用本身属于执行输入，但被引用的外部状态不属于初始或最终环境。

模型 API、公共网络、第三方数据库和宿主文件系统可能影响结果，却无法全部由 RunLab 控制。实验方负责判断它们是否满足对照条件。RunLab 忠实保存输入中的引用和执行时直接观察到的事实，不推断外部内容身份，也不声称捕获整个世界。

程序可以把自己对外部依赖的观察写入 `stdout`、`stderr` 或受控文件系统。RunLab 保存相应的原始结果，但不把程序的观察误写为外部系统的完整状态。

## 不理解程序内部流程

只有实验发起者知道哪些工具调用、消息、记忆、规划或 Trace 值得保留。RunLab 不为这些 Agent 领域概念建立内置模型。

程序自行决定把所需信息写入标准流或受控文件系统。RunLab 负责执行、捕获和保存，不解释其领域语义。因此，Agent Loop 是 RunLab 的典型场景，而不是限制产品边界的特殊协议。

## Experiment 位于上层

RunLab 是单条 Run 的源事实所有者。上层实验系统可以按 `run_id` 选择和组织多条 Run，形成参数矩阵、重复采样、评分、报告或 Experiment 实体。

一次 Run 可以被多个 Experiment 重复使用，Experiment 的比较方法和判断也可以事后改变。RunLab 提供可以被理解和比较的样本，但不判断两次 Run 是否构成有效对照，也不替使用者推断因果关系。

稳定原则见 [RunLab 设计原则](/design/generated/principles)，纯执行契约见 [Run Protocol](/design/generated/run-protocol)，持久资产见 [RunLab Run 资产与身份](/design/generated/run-assets)。
