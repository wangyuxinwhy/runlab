---
title: "Native Linux Backend"
description: "定义 Native Linux 主要执行路径如何实现 OCI 执行、区分内外部宿主资源、捕获事实并形成最终环境。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# Native Linux Backend

`NativeEngine` 是 RunLab 在 Linux 上的 reference execution implementation。它直接按照经过验证的 OCI Image 和 OCI Runtime Configuration 运行程序。

本页定义稳定的实现责任。Runtime 版本、受支持字段子集、主机矩阵和验证结果由代码仓库拥有。

## 执行管线

```text
verified Initial Manifest
→ verified ordered Layer render
→ private rootfs
→ OCI bundle and config.json
→ OCI Runtime
→ process and stream facts
→ stopped filesystem capture
→ Final OCI Image
```

初始环境、OCI Runtime Configuration 和最终环境保持标准 OCI 语义。具体使用哪个 OCI Runtime 由 Engine 构造配置决定，不能成为另一套 `RunInput`。

`NativeEngine` 只在 Linux 构建。macOS 使用受管理的 Linux VM 时，实际执行者仍是虚拟机内的同一个 `NativeEngine`，不是另一种 VM Engine。

## 文件系统实现

Engine 从经过完整校验的有序 Layers 构造私有 rootfs。可写执行视图不能跨 Program 或跨调用共享。已经展开的只读下层可以作为随时能够重建的缓存。

Program 停止并卸载运行时文件系统后，Engine 比较受控文件系统在执行前后的状态。检测到的变化进入唯一的逐字节安全变更集和 Layer 编码路径。Engine 不能自行定义另一种 Layer 编码，也不能在捕获失败后暗中替换执行机制。

## 进程监督

OCI Runtime 子进程边界负责：

- 分别观察 Program 的创建、启动和真实停止状态。
- 准确传递 `stdin`，并持续排空 `stdout` 和 `stderr`。
- 每个输出流最多保留 100 MiB，并处理执行超时和调用级取消。
- 在有限时间内停止整个进程树。
- 在有限时间内等待 Runtime 和辅助进程。
- 记录资源清理事实。

退出码 137 或 `SIGKILL` 不能单独证明 OOM。只有能够明确归属于该 Program 的 cgroup 计数器和可信基线，才能形成 OOM 事实。

## 宿主资源

Engine 为一次调用创建的 rootfs mount、cgroup、network namespace、veth、DNS 配置和 Runtime root 是调用内私有资源。每次并发调用使用独立名称和生命周期，并在 `run` 返回前按明确顺序完成有界清理。

调用方在 OCI Runtime Configuration 中声明的 bind mount、namespace path、hook 和其他宿主资源不归 Engine 所有。Engine 必须忠实使用这些字段，但清理时只能撤销自己建立的连接或挂载动作，不能删除、重置或回滚被引用的外部资源。

无法证明资源所有权时，Engine 不得擅自操作。Rust 的 `Drop` 只用于尽力释放局部资源，不能代替显式终结流程。Engine 进程或宿主机意外消失时，`run` 可能没有返回值；当前实现契约不增加持久身份或恢复接口。

## 网络

`network=isolated` 为整次执行提供没有外部连接的私有网络。主程序和受控依赖程序位于同一受控 network namespace 时，可以通过 loopback 通信。这是一种实现机制，不是受控依赖程序的 readiness 协议。

`network=egress` 在同一私有网络上增加向外发起连接的能力。它不提供从宿主机进入 Program 的端口发布，也不允许一次执行访问其他执行的私有地址。临时 DNS 配置属于执行资源，必须在捕获最终环境前验证移除。

## Rootful 与受限 rootless

Rootful profile 是完整的 reference path，可以使用内核隔离、OverlayFS、cgroup 和 Engine 创建的宿主资源。

Rootless 是同一个 `NativeEngine` 的受限能力 profile，不是另一种 Engine。只有能够忠实实施输入的 OCI 语义时才能接受执行。无法实现的 resource、mount、network 条件或责任归属必须在任何 Program 启动前以字段路径和原因返回 `EngineError`，不能用近似行为换取执行成功。

## 多 Program

`NativeEngine` 为每个 Program 创建独立可写 rootfs、标准流和结果槽位。它先创建并启动全部受控依赖程序，全部成功执行 OCI `start` 后再启动 `primary`。

Engine 不实现通用 readiness probe。依赖 Program 在成功启动后退出不会阻止 `primary` 启动；它在 `primary` 执行期间退出也不会触发重启或单独终结。进入终结阶段后，所有仍在运行的 Program 共享协议规定的停止宽限期，并分别捕获最终环境。

## 验证

Native 验证必须在真实 Linux OCI Runtime 环境完成，并至少覆盖 rootfs、mount、network、cgroup、标准流、非零退出、signal、timeout、取消、最终 Image、并发隔离和调用内 cleanup。macOS 交叉编译或单元测试不能代替这条真实执行证据。
