---
title: "RunLab 术语表"
description: "统一 Run Protocol、RunLab 持久化模型、OCI 对象与实验术语的中英文和责任边界。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# RunLab 术语表

本页统一 RunLab 文档中反复出现的中文称呼、英文名称和字段写法。完整语义由对应协议或架构页定义。

## 使用规则

- 中文正文优先使用自然、容易理解的表达。容易理解胜于表面简洁。
- JSON 字段、枚举值、命令、文件名和代码标识保持英文并使用代码格式。
- Run、RunLab、OCI 和 `digest` 保持原文。不要把 Run 泛化为“运行”，也不要把 `digest` 翻译成“摘要”。
- OCI 标准对象使用官方英文名称，例如 OCI Runtime Configuration。中文句子说明它做什么，不逐词硬译。
- 同一概念只使用一个稳定名称。协议对象与 RunLab 的持久化对象必须明确区分。

## Run Protocol 术语

| 推荐写法 | 英文名称 | 含义 |
| --- | --- | --- |
| Run Protocol | Run Protocol | 定义 `RunInput` 如何由 Run Engine 执行并返回 `RunOutput` 或 `EngineError` 的协议。 |
| Run 输入 | `RunInput` | 已经解析完成、可以直接执行的完整输入。公开类型写作 `RunInput`。 |
| Run Engine | `RunEngine` | 验证并执行 `RunInput`、返回协议结果的执行边界。 |
| Run 输出 | `RunOutput` | 一次执行中取得的 Program、标准流、控制和最终环境事实。 |
| 引擎错误 | `EngineError` | 引擎无法忠实开始执行，或无法形成协议有效的 `RunOutput` 的错误。 |
| 程序 | Program | 一次执行中由 Run Engine 创建、启动、停止并收集结果的程序。 |
| 主程序 | Primary Program | `program_id` 固定为 `primary` 的主要 Program。 |
| 受控依赖程序 | Managed Dependency Program | 在 `primary` 之前启动、由同一 Run Engine 管理的其他 Program。 |
| 执行控制 | execution controls | `RunInput` 中在 OCI Runtime Configuration 之外补充的执行超时和网络条件。 |
| 执行事实 | execution fact | Run Engine 直接观察到或能够证明的事实。 |
| 操作错误 | operation error | 引擎在准备、创建、启动、传输、停止、捕获或清理时观察到的错误。它与 Program 的进程结果分开记录。 |

## RunLab 产品术语

| 推荐写法 | 英文名称 | 含义 |
| --- | --- | --- |
| Run | Run | 一条执行输入、执行事实和执行结果都被明确记录，因而能够被理解和比较的程序执行。 |
| Run 记录 | Run Record | RunLab 以 `run_id` 保存的持久记录，包含输入、生命周期和最终 `completion`。 |
| Run 资产 | Run Asset | 完整 Run Record 与其引用的初始、最终 OCI Image 内容。 |
| Run 中断 | `RunInterruption` | RunLab 证明原引擎调用不再运行且不可能返回后，保存的中断原因、证据和不可取得结果。它是与 `EngineError` 分别解释的持久化完成形态。 |
| 接受 | acceptance | RunLab 在执行前原子保存 `run_id` 与 `RunInput` 的持久边界。 |
| 协调器 | Coordinator | 构造并持久化 Run、调用 Run Engine、发布结果和组织恢复的 RunLab 组件。 |
| 执行后端 | Backend | Run Engine 内部用于调用 Runtime、进程、标准流和文件系统机制的范围有限的实现边界。 |
| 恢复 | recovery | RunLab 根据持久证据处理没有正常闭合的执行和资源。 |

`run_id` 是 RunLab 的持久身份。Run Engine 的执行接口以 `RunInput` 和返回结果为边界。

## OCI 与内容身份术语

| 推荐写法 | 英文名称 | 含义 |
| --- | --- | --- |
| 初始环境 | Initial Environment | Program 启动前的受控文件系统，由 `RunInput.initial_environment` 中的 OCI Image Descriptor 标识。 |
| 最终环境 | Final Environment | Program 停止后的受控文件系统，由 `RunOutput` 中的 OCI Image Descriptor 或不可取得原因表达。 |
| OCI Runtime Configuration | OCI Runtime Configuration | 描述进程、用户、工作目录、`mount`、`namespace` 和资源条件的标准 `config.json`。 |
| Image Manifest | Image Manifest | 引用一份 Image Config 和有序 Layers 的 OCI 对象。 |
| Descriptor | Descriptor | 使用 `mediaType`、`digest` 和 `size` 标识 OCI 内容的结构。 |
| `digest` | digest | 基于确切内容字节计算出的内容身份，例如 `sha256:...`。 |
| Layer | Layer | OCI Image 中按顺序应用的一组文件系统变化。 |
| bind mount | bind mount | OCI Runtime Configuration 声明的宿主文件系统挂载。 |
| OCI Image Store | OCI Image Store | 按 `digest` 保存初始和最终环境所需 OCI Image 对象的内容存储。 |
| Local Image Catalog | Local Image Catalog | 把可变名称或 `tag` 映射到 OCI Image Descriptor 的本地索引。 |

## 实验术语

| 推荐写法 | 英文名称 | 含义 |
| --- | --- | --- |
| 实验 | Experiment | 选择、组织、比较和评价多条 Run 的上层活动或实体。 |
| 参数矩阵 | Matrix | 按多个变量组合生成多次执行请求的实验组织方式。 |

## 写作示例

协议层可以写：

> Run Engine 接收 `RunInput`，并返回 `RunOutput` 或 `EngineError`。

产品层可以写：

> RunLab 验证调用方生成的 `run_id`，保存 `RunInput`，调用 Run Engine，再把返回结果发布为 Run Record。

两句话描述的是相邻但不同的责任，不能把 `run_id` 或持久状态写回协议接口。
