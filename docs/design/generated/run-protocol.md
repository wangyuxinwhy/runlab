---
title: "Run Protocol"
description: "定义 RunInput、RunEngine::run、RunOutput 与 EngineError 组成的纯程序执行协议。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# Run Protocol

Run Protocol 是建立在 OCI 标准对象之上的程序执行协议。它只回答一个问题：给定一份明确的执行输入，执行引擎应当如何运行程序，并返回哪些执行结果。

本文及后续协议页中的“必须”“不得”和“可以”是规范性要求。示例只用于解释要求，不能替代规范本身。

## 协议模型

```text
RunInput
   │
   ▼
RunEngine::run
   │
   ▼
Result<RunOutput, EngineError>
```

- `RunInput` 是一次执行所需的完整输入。
- `RunEngine::run` 负责验证输入、确认自身能力、执行所有 Program，并尽力收集结果。
- `RunOutput` 保存这次调用已经取得的执行事实与结果。
- `EngineError` 表示引擎没有产出一份符合协议的 `RunOutput`。

调用方负责提供完整的 `RunInput`，并消费本次调用返回的 `Result`。协议对象的语义不依赖某种传输或存储实现。

## 协议目标

一次程序执行可以抽象为：

```text
（初始环境，执行配置） → 程序执行 → （执行后环境）
```

Run Protocol 使这条边界足够明确。输入完整描述受控的执行条件，执行引擎忠实实施这些条件，输出保存引擎能够在执行边界上直接观察到或取得的事实。协议不解释被执行程序的内部领域语义。

初始环境由 OCI Image 表达，进程及其运行条件由 OCI Runtime Configuration 表达。协议只补充 OCI 没有覆盖、但一次可比较的执行需要的少量语义：长度有限的标准输入、敏感环境变量与文件、输出捕获、执行超时、网络控制、停止流程、可选的最终环境捕获和多个 Program 的协调。

## 核心对象

| 对象 | 含义 |
| --- | --- |
| `RunInput` | 已经解析完成、可以直接交给执行引擎的完整执行输入。 |
| `Program` | 本次调用中由引擎执行的程序。输入中必须有一个 `primary`，还可以有零个或多个受控依赖程序。 |
| `RunEngine` | 实施本协议的执行边界。它验证输入与自身能力，管理执行生命周期并收集结果。 |
| `RunOutput` | 本次调用取得的执行事实与结果，包括 Program 的创建、启动、进程、标准流、停止动作，以及最终环境是否请求和相应结果。 |
| `EngineError` | 引擎无法开始符合协议的执行，或无法形成一份结构完整、事实可信的 `RunOutput`。 |

## `RunOutput` 与 `EngineError` 的边界

Program 的非零退出、终止信号、执行超时、取消、OCI `create` 或 `start` 失败、标准流传输错误、输出截断、停止错误和最终环境不可用，都是一次执行可能产生的结果。即使第一个 Program 没有成功启动，只要引擎能够形成符合协议的事实集合，就必须返回 `Ok(RunOutput)`，不能仅因为这些结果“不成功”而返回 `EngineError`。

下列情况属于 `EngineError`：

- `RunInput` 不符合协议，所引用的必要 OCI 内容无法取得，或者引擎在开始执行操作前已经知道自己无法忠实执行它。
- 引擎自身失败，以至于无法形成一份结构完整、事实可信的 `RunOutput`。

一旦有 Program 开始执行，引擎必须尽力停止其余进程、排空标准流，并按 `RunControls` 决定是否捕获最终环境，然后返回 `RunOutput`。如果引擎进程意外终止或宿主机停止运行，这次调用可能没有返回值。`Result` 只表示调用实际返回的协议结果。

## 协议不变量

所有实现都必须满足以下要求：

1. `RunInput` 必须是确定的执行对象，不能包含需要执行引擎临时解析的名称、`tag` 或产品默认值。
2. 引擎必须在启动任何 Program 前验证输入，并确认自身能够忠实执行。已知不支持的条件必须返回 `EngineError`，不能静默忽略、改写或降级。
3. 调用开始后，引擎不得修改 `RunInput`。执行时生成的 `bundle`、`rootfs` 绝对路径、Runtime 对象名和其他临时资源不属于输入。
4. 引擎只能记录自己直接观察或能够证明的事实。无法取得的结果必须明确标记为不可用，不能猜测、补齐或用空值冒充。
5. Program 的进程结果、引擎执行操作时发生的错误和外部系统作出的评价必须分开表达。
6. 每次调用必须有且只有一个主程序，可以有零个或多个受控依赖程序。
7. OCI 已经定义的字段、默认值和 `options` 继续由相应 OCI 标准及所用 Runtime 解释。Run Protocol 只规定它们与本协议对象相接的语义。
8. OCI Runtime Configuration 显式引用的宿主资源属于输入。被引用资源的实际状态不因此成为受控环境，也不由引擎推断内容身份。

## 协议文档结构

协议按以下顺序展开：

1. [Run Input](/design/generated/run-input)：输入结构、OCI Runtime Configuration、标准流限制和执行控制。
2. [Run Engine](/design/generated/run-engine)：`run` 的执行顺序、验证边界、计时、终结和结果收集。
3. [Run Output 与 EngineError](/design/generated/run-output)：输出结构、事实分类，以及执行结果与引擎错误的边界。
4. [Run 的初始环境与最终环境](/design/generated/run-environments)：OCI Image 如何表达执行前后的受控文件系统。
5. [受控依赖程序的执行语义](/design/generated/managed-programs)：多个 Program 的启动关系和生命周期协调。

## 与调用方组合

采用本协议的系统负责构造 `RunInput`、调用 `RunEngine`，并消费返回结果。系统可以把结果传给后续处理，也可以组织多次调用进行比较。无论采用哪种方式，同一次 `run` 的协议语义保持不变。

Program 自行决定把哪些内部信息写入 `stdout`、`stderr` 或受控文件系统。`RunEngine` 保存执行边界上可取得的原始结果，不解释工具调用、消息、Trace 或任务阶段等领域内容。
