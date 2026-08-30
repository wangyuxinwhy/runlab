---
title: "macOS Managed Linux VM"
description: "定义 macOS CLI 与本地 Linux 虚拟机之间的 State、OCI 内容、Run 数据、宿主路径和可恢复操作边界。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# macOS Managed Linux VM

macOS 不能直接执行 Linux OCI bundle。RunLab 在受管理的本地 Linux VM 中运行同一个 Linux Run Engine，而不是在 macOS 宿主机上建立另一套执行协议。

本页只定义稳定的宿主机与虚拟机边界。VM provider、固定版本、安装要求和验证结果由代码仓库拥有。

## State 留在 Linux 虚拟机

OCI Layout、Run Database、`rootfs`、Runtime 状态、运行时挂载和最终环境全部位于 Linux 虚拟机的文件系统。macOS 路径不能被伪装成虚拟机路径，也不能通过共享目录成为容器 `rootfs`。

macOS 侧只选择一个明确的虚拟机 State。OCI 内容和 Run Record 的导入、导出与迁移都通过显式字节传输完成。

## 同一个执行引擎

虚拟机运行与原生 Linux 相同版本的 `runlab` 二进制文件，并实施同一 Run Protocol。macOS CLI 负责虚拟机生命周期、实现版本与能力的握手，以及字节传输，不重新实现 OCI Image、OCI Runtime Configuration、Run Engine 或最终环境。

每次操作前，macOS CLI 与虚拟机校验 RunLab 实现版本、受支持的协议能力、操作系统和体系结构。身份或能力不匹配时拒绝操作，不能尝试兼容性降级。这种握手是 RunLab 的传输契约，不是在 `RunInput` 中增加版本字段。

## OCI 内容与 Run 数据传输

macOS 与虚拟机之间只传输边界明确的 OCI 内容、Run Record 和命令结果。具有 OCI Descriptor 的内容按 `digest` 和 `size` 复验，其他数据按对应格式和完整性规则验证。传输不能改写已经持久化的 `RunInput`。

Secret 来源在 macOS 请求构造层解析。Secret 值通过 mode 0600 的唯一临时文件传入虚拟机，值本身不进入进程参数。虚拟机内 RunLab 从这些文件构造 Protocol `Secrets`；临时文件由持有 Run 的 transient service 在结束后删除。这个传输只改变 Secret 值到达同版本 RunLab 的方式，不改变 `RunInput` 的执行语义。

传输结果采用不覆盖已有内容的原子发布。连接中断时，虚拟机中的 Run 继续由独立 Coordinator 持有。调用方可以重新连接并读取同一条 Run，不能因为传输失败而自动重跑一条可能已经产生副作用的执行。

`run start` 的实时阶段和 Program 标准流遵循 [RunLab Run 实时观察与 CLI 输出](/design/generated/live-events)。Guest 产生的 stderr NDJSON 必须在到达 Host 时立即转发，不能缓存到 Guest 命令结束，也不能由 Host 重新解释或改写 Program payload；Host 自己观察到的连接问题作为传输诊断加入同一输出通道。

OCI Runtime Configuration 中的宿主路径按照 Linux 虚拟机的宿主命名空间解释。macOS 本机路径不能原样在虚拟机中实施。RunLab 的请求构造层应当在调用 Run Engine 前发现并拒绝这类输入。引擎仍会验证自己实际收到的路径和能力。传输层不得把 macOS 路径改写成另一个虚拟机路径。

调用方如果要把 macOS 文件作为受控环境的一部分使用，应先把内容导入 OCI Image；如果要引用虚拟机中的外部路径，则必须明确提供虚拟机路径并自行保证其状态。

## 执行所有权与取消

持久 Run 由虚拟机中的 Coordinator 持有。SSH 或控制连接意外断开不表示取消，也不能决定 Run 是否结束；调用方可以按同一个 `run_id` 读取已经发布的事实，并通过 `runlab run cancel RUN_ID` 向虚拟机中的 Coordinator 持久提交取消请求。

`run start` 或 `exec` 仍在前台连接时，调用方送达 macOS CLI 的 `SIGINT` 或 `SIGTERM` 是显式取消。Host 必须把信号准确送到持有本次调用的 Guest RunLab，并等待其完成 Engine 的有界终结流程，不能只关闭本地转发进程。Guest 内部的 service unit、进程号和传输标识只用于这次控制，不成为公开身份。

`exec` 没有 `run_id` 或后续读取面。Host 被强制终止、VM 故障或连接在没有显式信号时丢失，不能被解释为已经取消，也不能把稍后的结果恢复为 Run。传输层必须尽力清理自己持有的临时输入，但不能伪造执行结果。

## VM 生命周期

创建、启动、安装和状态检查是不同操作。状态检查只报告前置条件事实，不会在读取状态时修复 VM。安装动作必须验证传入的 Linux 二进制文件和 OCI Runtime，再建立 reference profile。

VM Image、Runtime 和 macOS 工具的具体版本可以变化，但必须通过带版本的实现契约固定下来，并且可以检查。

## 安全边界

Managed VM 不提供虚拟机 shell 直通或任意 argv 转发，也不让目标容器接触虚拟机控制通道或 RunLab State。Host 只从已经解析的公开操作构造固定的 typed Guest 调用；用于传递暂存输入位置的私有参数不是通用命令入口。所有扩展都必须保持这条最小传输边界。
