---
title: "Run Output 与 EngineError"
description: "定义 RunOutput 的执行事实结构，以及执行结果与 EngineError 的明确分界。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# Run Output 与 EngineError

`RunOutput` 保存一次 `RunEngine::run` 调用取得的执行事实与结果。它由整次调用的 `execution` 和按 `program_id` 组织的 `programs` 构成。调用方结合原始 `RunInput` 理解这些结果。

本页同时定义 `EngineError` 与执行结果的分界。

## 输出结构

```text
RunOutput
├── execution
│   ├── started_at
│   ├── ended_at
│   ├── timed_out
│   ├── cancelled
│   └── errors
└── programs
    ├── primary
    │   ├── create
    │   ├── start
    │   ├── process
    │   ├── stdin
    │   ├── stdout
    │   ├── stderr
    │   ├── stop_actions
    │   ├── final_environment
    │   └── errors
    └── ...
```

`programs` 必须为输入中的每个 `program_id` 保留一项，即使相应 Program 没有启动。`program_id` 已经是映射键，不在值中重复。

这里给出语义结构，不强制某种 JSON 编码。具体表示必须保留同样的信息和缺失原因，不能用字段形状掩盖事实差异。

## `execution`

`execution` 保存整次调用的事实：

- `started_at` 和 `ended_at` 是引擎直接观察到的执行区间边界。没有进入执行区间时，必须明确不可用。
- `timed_out` 只在执行期限实际到达时为真，不能根据进程退出码推断。
- `cancelled` 只在调用方的取消请求实际使调用进入终结时为真。
- `errors` 保存无法归属于单个 Program 的准备、协调、计时或清理错误。

时间使用明确时区的墙上时钟记录观察点。超时判断使用单调时钟。墙上时间不能代替持续时间的计量依据。

## Program 的创建、启动和进程结果

`create` 与 `start` 分开保存 OCI 生命周期事实：

- `create` 表示执行环境是否建立，以及 Runtime 返回的结果。
- `start` 表示用户进程是否启动，以及能够观察到的启动时间。
- `process` 表示初始进程实际如何结束，包括退出码或终止信号，以及结束时间。

无法证明用户进程是否启动或结束时，必须明确标记未知并记录原因。不能用 `not_started`、退出码 `0` 或空对象代替缺失证据。

退出码为零和非零都是已经取得的进程结果。OCI `create` 或 `start` 失败也属于相应 Program 的执行事实，即使第一个 Program 从未成功启动。超时、取消和引擎发送的停止信号是独立事实，不能从退出码推导，也不能覆盖 Runtime 实际报告的结果。

## 标准输入结果

Program 的 `stdin` 结果至少说明：

- 实际成功写入多少字节。
- 是否在全部字节写完后成功关闭输入写端，使 EOF 可供 Program 读取。
- Program 提前关闭输入或写入失败时，实际观察到的错误。

输入原始字节属于 `RunInput`，不在 `RunOutput` 中重复。

## 标准输出与标准错误

`stdout` 和 `stderr` 分别包含：

- 从流开头保留的原始字节。
- 是否因为固定的单流 100 MiB 上限而省略了后续字节。
- 是否观察到 EOF。
- 捕获时发生的读取错误。

恰好保留 100 MiB 且没有更多输出时不算截断。协议不合并两个流，也不推断它们之间的逐字节或逐行顺序。

## 停止动作

`stop_actions` 按实际尝试顺序保存引擎对 Program 采取的停止动作。每项至少说明信号、尝试时间，以及 Runtime 是接受操作还是返回错误。

`stop_actions` 说明引擎做了什么，`process` 说明进程最终如何结束。已经退出或确认没有启动的 Program 不应出现伪造的停止动作。

## 最终环境

`final_environment` 明确区分三种状态：

| 状态 | 含义 |
| --- | --- |
| `captured` | 包含指向完整 OCI Image 的 Descriptor。 |
| `unavailable` | 调用方请求了捕获，但最终环境无法取得，并保存原因。 |
| `not_requested` | `RunInput.controls.capture_final_environment` 为假，引擎没有尝试构造或发布最终 Image。 |

请求捕获时，Program 非零退出、超时、取消或被强制终止不会自动使最终环境无效。只要停止后的受控 `rootfs` 能被忠实捕获，就必须返回实际得到的 Image。捕获失败时，不能用初始环境、空环境、部分结果或 `not_requested` 冒充最终环境。未请求捕获时，即使技术上能够取得，也必须返回 `not_requested`，不能产生未要求的 OCI 内容副作用。

## 操作错误

`execution.errors` 和每个 Program 的 `errors` 分别保存相应范围内的操作错误。错误至少包含观察时间、阶段和底层操作返回的信息。父级已经确定范围时，不重复 `program_id` 或额外的 `scope` 字段。

操作错误与 Program 进程结果可以同时存在。OCI `create`、OCI `start`、`signal`、`wait`、标准流读写、运行时文件系统移除、环境捕获或清理失败，只要仍能形成可信结构，都必须进入 `RunOutput`。Program 退出码为 `0` 时仍可能发生输出读取或环境捕获错误。Program 退出码非零时也可能拥有完整标准流和最终环境。

## `EngineError`

`EngineError` 至少区分以下原因：

- 输入不符合协议，并能定位到相应字段或约束。
- 当前引擎不支持某项输入，无法忠实执行。
- 引擎内部失败，无法形成一份符合协议且事实可信的 `RunOutput`。

`EngineError` 描述引擎为什么没有返回协议结果，不是 Program 的“失败结果”。引擎不得仅因为非零退出、信号、超时、取消、截断、捕获失败或清理失败而放弃已经能够形成的 `RunOutput`。

## 事实边界

`RunOutput` 只保存执行边界上取得的事实。分数、排名、成功判定、因果解释和 Experiment 归属由外部系统产生。

调用方在 OCI Runtime Configuration 中引用的宿主资源属于输入。外部资源的内容、最终状态和业务可用性不属于 `RunOutput`，除非 Program 自己通过标准流或受控文件系统明确留下观察结果。
