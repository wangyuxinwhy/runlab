---
title: "RunLab Run 资产与身份"
description: "定义 RunLab 在协议之外建立的 run_id、Run Record、Run Asset 与幂等创建语义。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# RunLab Run 资产与身份

本页定义 RunLab 如何在 Run Protocol 之外为一次执行建立持久身份和可复用资产。

Run Protocol 的核心接口只是：

```text
RunInput → RunEngine::run → Result<RunOutput, EngineError>
```

RunLab 在这一接口之外接收并验证调用方生成的 `run_id`，持久化输入、执行状态和返回结果，从而让一次调用成为能够被长期读取和比较的 Run。

## RunLab 中的 Run

RunLab 的一条持久 Run 至少包含：

```text
Run
├── run_id
├── metadata
├── input: retained RunInput facts
├── lifecycle 与时间事实
├── cancellation_requested_at
└── completion
    ├── engine_returned
    │   └── Result<RunOutput, EngineError>
    └── interrupted
        └── RunInterruption
```

`engine_returned` 原样保存 Run Engine 的返回结果。`interrupted` 表示 RunLab 已经证明原调用不可能再返回，并以 `RunInterruption` 保存中断原因、观察时间、证据来源和无法取得的结果。如何协调和证明中断见[Run 协调与持久化边界](/design/generated/coordination-and-recovery)。`RunInterruption` 属于 RunLab 的持久模型，不冒充 `EngineError`。

## Run metadata

调用方可以在创建 Run 时提供用于理解和选择 Run 的 metadata：

```json
{
  "description": "Replay SWE-bench django__django-11099 with pi",
  "labels": {
    "agent": "pi",
    "suite": "swe-bench",
    "task": "django__django-11099"
  }
}
```

`description` 是可选的简短自然语言说明。`labels` 是调用方提供的任意字符串键值对，RunLab 不预定义或解释 key 的领域含义。metadata 表达调用方的创建意图和检索线索，不是执行事实，也不替代 `RunInput`、`RunOutput` 或外部评价。

Run metadata 在 Run 被接受时固定并保存在 Run Record 中。它不进入 Run Protocol，不交给 Run Engine，也不改变协议输入的相等语义。为了避免同一个 `run_id` 在重试时被静默绑定到不同描述，同一次幂等创建要求 metadata 也相同；修改已有 Run 的 metadata 不属于创建接口。

## `run_id`

`run_id` 标识 RunLab 中的一次持久执行。它由调用方在创建 Run 时生成，采用规范字符串形式的 UUID v4。

`run_id` 由调用方生成，使请求在网络重试前就有稳定引用。UUID v4 不携带实验名称、时间、顺序或其他可变业务语义，避免身份随后因解释变化而失真。需要人类可读的名称、标签或 Experiment 归属时，应作为独立元数据保存。

同一个 `run_id` 只表示同一次创建意图。重复实验必须使用新的 `run_id`，即使 `RunInput` 完全相同。

## 幂等创建

RunLab 可以用 `run_id` 提供幂等创建：

- ID 不存在时，创建并保存输入。
- ID 已存在且输入按 Run Protocol 的字段语义相同、Run metadata 也相同时，返回现有 Run。
- ID 已存在但输入或 Run metadata 不同时，报告身份冲突。

输入是否相等，按照 [Run Input](/design/generated/run-input) 的比较规则判断：JSON 对象成员顺序不参与比较，数组顺序保留语义，`stdin` 和 Secret 比较原始字节，环境比较完整 Descriptor。RunLab 不依赖某一种 JSON 序列化字节来判断幂等。产品不保存 Secret 原文时，可以用不对外返回的内容摘要完成同一次创建内的相等判断。

这一规则帮助调用方安全重试 RunLab API，不是 `RunEngine::run` 的语义。引擎每被调用一次，都会执行一次对应的输入。

## Program 身份

`program_id` 是 `RunInput.programs` 和 `RunOutput.programs` 的映射键，标识同一次执行中的程序角色。主程序固定为 `primary`。其他 Program 使用调用方提供的唯一键。

`program_id` 不标识跨 Run 的进程实例。不同 Run 可以复用相同键来表达相同角色，但每次执行仍是独立样本。

## Run Record 与 Run Asset

Run Record 是 RunLab 保存的结构化持久记录，直接拥有 `run_id`、可持久化的输入事实、执行状态、可空的首次取消请求时间、`completion`，以及 RunLab 为协调和状态核验保存的必要事实。取消请求时间记录调用方意图，不证明 Engine 已经停止；最终取消和停止事实仍由 `completion` 中的 Engine 返回值拥有。Run Record 不因此必须逐字节保存完整的协议对象。

RunLab 不在公开 Record 中保存 `RunInput` 的 Secret 值，只保存环境变量名、文件目标和 `retained: false`。因此 Secret-dependent Run Asset 能证明当时声明了哪些 Secret 槽位和取得了哪些执行结果，但不能单独恢复 Secret 或重新构造逐字节相同的 `RunInput`。重复执行时必须由调用方重新提供 Secret。

Run Asset 是一条可以独立保存、复制和验证的 Run 资产，由以下内容组成：

- 完整的 Run Record。
- `RunInput` 和 `RunOutput` 中环境 Descriptor 引用的全部 OCI Image 内容。

除初始和最终环境的 OCI Image 内容外，标准输入、标准输出、标准错误、进程结果、时间和错误均由 Run Record 直接保存。OCI Content Store 只按 Descriptor 保存 Image Manifest、Image Config 和 Layers。Program 如果把 Secret 写入标准流或可写文件系统，相应字节会按普通执行结果保存；RunLab 不扫描任意输出来推断 Secret。

## OCI 内容身份

OCI Descriptor 的 `digest` 标识一段确切的 OCI 对象字节，不标识 Run、Program 或环境角色。同一个 OCI Image 可以被多条 Run 复用。初始环境和最终环境即使引用相同 Descriptor，也仍然承担不同角色。

如果 Record 已经引用某个 Descriptor，而相应内容无法取得，Run Asset 不完整。取得的字节与 `size` 或 `digest` 不符时，Run Asset 已损坏。RunLab 不得以其他 Image 替换、删除引用或改写原始事实来掩盖存储损失。

`RunOutput` 明确说明最终环境不可取得时，这项不可用事实本身是完整结果，并不意味着缺失了一份已经引用的 Image。

## 比较边界

| 问题 | 判断依据 |
| --- | --- |
| 是否是 RunLab 中同一次持久执行 | `run_id` 相同 |
| 是否具有相同的协议输入 | 按 Run Protocol 的字段语义比较，所有字段均相同 |
| 是否引用相同 OCI 内容 | 对应 Descriptor 的 `digest` 相同 |
| 是否得到相同的已记录完成事实 | 对应 `engine_returned` 或 `RunInterruption` 事实相同 |

相同输入的多条 Run 不能合并。实验需要的正是这些身份不同、可以独立比较的执行样本。
