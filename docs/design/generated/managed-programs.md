---
title: "受控依赖程序的执行语义"
description: "定义主程序与零个或多个受控依赖程序的启动关系、执行协调和独立结果。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# 受控依赖程序的执行语义

本页规定一次 `run` 调用包含多个 Program 时，受控依赖程序与主程序之间的执行关系。

每份 `RunInput` 必须有且只有一个 `primary`，可以有零个或多个受控依赖程序。`RunEngine` 管理每个 Program 的执行生命周期，但不解释 Program 之间的业务关系，也不判断数据库、网站或其他依赖在领域意义上是否可用。

## Program 集合

所有 Program 构成一个以 `program_id` 为键的扁平映射。主程序的键固定为 `primary`。其他键由调用方指定，并在同一输入中保持唯一。

每个 Program 都有自己的初始环境、OCI Runtime Configuration、标准输入、进程、输出流、操作错误和最终环境。一个 Program 的环境或结果不能替代另一个 Program 的输出。

协议只定义一项 Program 间关系：全部受控依赖程序成功执行 OCI `start` 后，才能启动 `primary`。受控依赖程序彼此没有协议规定的先后顺序，也不形成由协议解释的依赖图。

## 输入与外部依赖

受控依赖程序采用与 `primary` 相同的输入结构：

```text
<program_id>
├── initial_environment
├── runtime_config
└── stdin
```

Program 对文件、数据库、网站或外部服务的依赖，由这些输入和程序自身逻辑表达。例如，地址可以写入参数或环境变量，文件可以通过 OCI `mount` 暴露，外部连接受整次调用的 `network` 控制。

`RunInput` 忠实保存明确出现的条件。外部服务执行时的状态、返回内容和变化不属于 Program 的初始或最终环境，也不能由引擎根据一个地址或路径推断。

## 创建与启动

引擎先为全部受控依赖程序准备执行环境，再创建并启动它们。不同依赖程序可以串行或并行处理，产生的事实分别记录在对应的 `program_id` 下。

OCI `start` 成功只证明用户进程已经启动，不证明它完成初始化，也不证明监听端口、数据库连接、文件或远端服务已经可用。Run Protocol 不定义通用 `readiness` 条件。

任一受控依赖程序无法创建或启动时，`primary` 不得启动。引擎进入终结阶段，停止其他已经启动且仍在运行的 Program，并把每个 Program 的实际结果写入 `RunOutput`。

受控依赖程序在成功启动后、`primary` 启动前自行退出，仍然满足“曾成功启动”这一协议条件。引擎保存它的进程结果，并继续启动 `primary`。

## 执行期间

受控依赖程序在 `primary` 运行期间可以持续运行，也可以自行结束。它提前退出时，引擎记录实际进程结果，但不因此停止 `primary`，也不重新启动该 Program。

需要等待领域条件时，由 Program 自己完成等待。例如，`primary` 可以等待数据库查询成功、HTTP 服务可用、文件出现或初始化任务结束。协议不把任何一种特定检查提升为通用执行语义。

Program 可以使用 OCI Runtime Configuration 和 Run 级网络控制允许的任意机制协作。地址、端口、`Unix socket`、共享 `mount` 和外部服务均由输入与程序约定，协议不增加专用寻址机制。

## 终结与结果

当 `primary` 退出或无法启动、执行超时、调用被取消，或者引擎错误使执行进入终结时，引擎停止所有仍在运行的受控依赖程序。每个 Program 使用相同的有界停止流程，在一次调用中最多启动一次。

`RunOutput.programs` 必须为输入中的每个 `program_id` 保留独立结果，即使相应 Program 没有启动或提前退出。每个 Program 从自己的初始环境演化到自己的最终环境。`bind mount` 的外部内容和修改不进入任何 Program 的最终环境。
