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

## 只读 Host Share

Managed VM 配置可以声明零个或多个只读 Host Share。每个 Share 只由调用方给出的稳定名称和一个已解析的 macOS 绝对目录组成；RunLab 为它派生唯一的 Linux Guest 路径 `/mnt/runlab-shares/<name>`。底层共享机制、固定版本和能力验证属于 Managed VM 实现契约，不进入 Run Protocol。

VM 配置拥有 macOS 路径到 Guest 路径的映射。OCI Runtime Configuration 只引用派生后的 Guest 路径，再按普通 OCI `bind mount` 把相应目录暴露给 Program。传输层不得把 Runtime Configuration 中的 macOS 路径猜测或改写成 Guest 路径，也不得为 `bind mount` 隐式打包、复制或解包 Host 目录。macOS 调用层只允许固定的 resolver scaffold 和已经声明的 Share 子树；其他显式宿主路径必须在 Run 接受前拒绝。

Share 配置变更只允许在 VM 已停止时显式应用。配置操作不得替调用方停止或启动 VM，也不能无声终止正在执行的 Run。VM 兼容性检查必须逐项验证有效 Share 与声明完全一致，并拒绝额外、可写、路径冲突或实现不支持的 Host mount。

Share 的设备级只读不能替代 OCI 配置中的 `ro`。调用方引用 Share 时仍必须显式声明只读 mount；缺少 `ro` 必须在 Program 启动前拒绝，不能依靠执行时文件系统错误表达策略。

Host Share 是调用方管理的外部状态。其内容不进入 Program 的初始或最终环境，RunLab 不为 Share 内容计算 digest，也不因为路径相同而证明两次执行看到相同字节。只读只阻止 Guest 修改 Host 内容，不能阻止 macOS 调用方并发修改。文件名大小写、权限和其他文件系统行为继续由 Host 文件系统与共享机制决定；配置检查应当报告已知的语义差异。需要内容身份、可移植性或完整 Linux 文件系统语义时，调用方应把内容导入 OCI Image。

同一份引用 Guest Share 路径的 OCI Runtime Configuration 在原生 Linux 上仍要求该机器自行准备相同路径。RunLab 不把外部路径引用描述成可移植执行输入。

## 执行所有权与取消

持久 Run 由虚拟机中的 Coordinator 持有。SSH 或控制连接意外断开不表示取消，也不能决定 Run 是否结束；调用方可以按同一个 `run_id` 读取已经发布的事实，并通过 `runlab run cancel RUN_ID` 向虚拟机中的 Coordinator 持久提交取消请求。

`run start` 或 `exec` 仍在前台连接时，调用方送达 macOS CLI 的 `SIGINT` 或 `SIGTERM` 是显式取消。Host 必须把信号准确送到持有本次调用的 Guest RunLab，并等待其完成 Engine 的有界终结流程，不能只关闭本地转发进程。Guest 内部的 service unit、进程号和传输标识只用于这次控制，不成为公开身份。

`exec` 没有 `run_id` 或后续读取面。Host 被强制终止、VM 故障或连接在没有显式信号时丢失，不能被解释为已经取消，也不能把稍后的结果恢复为 Run。传输层必须尽力清理自己持有的临时输入，但不能伪造执行结果。

## VM 生命周期

创建、启动、安装和状态检查是不同操作。状态检查只报告前置条件事实，不会在读取状态时修复 VM。安装动作必须验证传入的 Linux 二进制文件和 OCI Runtime，再建立 reference profile。

VM Image、Runtime 和 macOS 工具的具体版本可以变化，但必须通过带版本的实现契约固定下来，并且可以检查。

## 安全边界

Managed VM 不提供虚拟机 shell 直通或任意 argv 转发，也不让目标容器接触虚拟机控制通道或 RunLab State。Host 只从已经解析的公开操作构造固定的 typed Guest 调用；用于传递暂存输入位置的私有参数不是通用命令入口。所有扩展都必须保持这条最小传输边界。
