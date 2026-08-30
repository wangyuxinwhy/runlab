---
title: "Run Engine 实现契约"
description: "定义 Run Engine 的输入验证、OCI 执行、事实收集、最终环境、清理和恢复接口责任。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# Run Engine 实现契约

本页定义 `run_engine` package 必须满足的实现契约。规范语义由 [Run Protocol](/design/generated/run-protocol) 拥有，本页说明 Rust 接口、`NativeEngine` 和实现责任如何组织。

## Package 边界

`run_engine` 是只依赖 `run_protocol` 的 Rust library crate。它公开一个执行接口和一个具体实现：

```text
RunEngine
└── NativeEngine
```

- `NativeEngine` 是 Linux reference implementation。

macOS Managed Linux VM 不是第三种 Engine。macOS `runlab` 把操作交给 Linux VM 中的同版本 `runlab`，由虚拟机内的 `NativeEngine` 执行。

`run_engine` 不负责：

- 分配或解释 `run_id`。
- 接受请求、处理幂等创建或发布 Run Record。
- 解析 Catalog 名称、`tag`、远端引用或产品默认值。
- 把多个 Run 组织成 Experiment。
- 根据进程结果给出评分或成功判断。

## Rust 执行接口

公共接口保持同步和阻塞：

```rust
pub trait RunEngine: Send + Sync {
    fn run(
        &self,
        input: RunInput,
        cancellation: CancellationToken,
    ) -> Result<RunOutput, EngineError>;
}
```

`CancellationToken` 是一次调用的旁路控制，不属于 `RunInput` 或协议序列化内容。Token 可克隆、线程安全且取消操作幂等。丢弃 Token 不表示取消。Engine 不安装进程级 signal handler；把终端信号转换为取消请求属于调用层责任。

同一个 Engine 实例可以被多个线程并发调用。每次调用的 workspace、Runtime 对象、容器、进程、网络和取消状态必须隔离。实现可以生成仅在调用内使用的临时标识，但该标识不进入协议对象或持久身份。

## OCI 内容边界

构造 Engine 时注入范围窄的 `OciContentStore`。它只允许实现按完整 OCI Descriptor 读取确切字节，并按预期 Descriptor 原子发布新内容。

Store 不提供名称、`tag`、Catalog、枚举、删除、Run Record 或数据库能力。Engine 默认在每次读取时验证 `mediaType`、`digest` 和 `size`。Store 可以额外承诺：已成功验证和发布的内容在 Store 使用期间不可替换或删除；只有在这个承诺成立时，Engine 才能复用与完整 Descriptor 绑定的验证事实。发布 Final Image 时，Config 和 Layers 必须先可读取，Manifest 才能作为 `RunOutput` 中的 Descriptor 返回。

经过验证的 OCI 原始字节必须保留。解析后的 JSON 或 typed view 只用于验证和访问字段，不能通过重新序列化替代已有内容身份。

## 输入与能力验证

每次 `run` 都必须在启动任何 Program 前验证完整的 `RunInput`。实现至少检查：

- OCI Image 的内容完整、Descriptor 匹配且平台可执行。
- OCI Runtime Configuration 符合所声明的标准，并与标准流和网络控制相容。
- Secret 环境变量名和文件目标合法，并且不与 Runtime Configuration 中已有环境变量或 mount 目标冲突。
- `stdin`、有限的超时设置和 Program 集合满足协议约束。
- 当前平台和执行机制能够忠实实施所有显式字段。

不支持的字段必须通过 `EngineError` 返回准确路径和原因。实现不能删除字段、替换调用方提供的宿主路径、近似网络语义或采用更小的输出保留上限。

产品可以额外提供参考性的 capability inspection。无论预先查询得到什么结果，`run` 都必须根据调用时的内容、宿主资源和 Engine 能力重新验证。

## 执行机制责任

Engine 为每个 Program 提供独立可写的初始环境，并忠实实施 OCI Runtime Configuration。`NativeEngine` 创建 OCI bundle，并用 OverlayFS 把 Image 的只读 snapshot chain 与本次调用独占的 upperdir、workdir 合成为可写 `rootfs`。

Snapshot chain 是 `NativeEngine` 私有的启动缓存，以有序 DiffID chain 标识；它不是新的 Image、协议对象或持久身份。缓存不能取代 content store 中保留的原始字节。对承诺不可变的 Store，Engine 可以随 snapshot 保存精确绑定 Layer Descriptor、DiffID 和展开大小的验证收据；其他 Store 仍需逐次验证内容。每次调用拥有独立写层，不能修改共享 snapshot；Engine 可以随 snapshot 缓存初始 filesystem Inventory。请求捕获 Final Image 时，内容只从本次调用的稳定写层变化构造，并走公共 content-addressed publish 路径。

Program 没有 Secret 时，`NativeEngine` 把调用方提供的 Runtime Configuration 精确字节直接用作 bundle 的 `config.json`。存在 Secret 时，Engine 可以在私有 workspace 内派生一次仅供底层 Runtime 使用的 `config.json`：把 Secret 环境变量加入进程环境，把 Secret 文件作为 Engine-owned 临时源的只读 bind mount 加入配置。派生过程不修改 `RunInput`，不改变调用方配置的持久表示，也不把临时源路径暴露为协议输入。

实现生成的 bundle 路径、容器名、PID、socket 和临时 mount 是调用内资源。调用方在配置中显式声明的宿主资源必须忠实执行，两者不能相互替换。

底层机制的 `create`、`start`、`signal` 和 `wait` 结果必须按实际证据进入对应 Program 输出。实现不能从后续结果反推一个没有观察到的早期阶段。

并发调用可以共享只读 snapshot，但不能让宿主级可变资源的多步操作交错。例如一组网络规则必须以一次调用为单位原子发布和撤销，锁只覆盖这些宿主变更，不能串行化 Program 执行。宿主级进程事件订阅只能在本次调用的准备辅助进程结束后建立，避免把 Engine 自己创建的 `ip`、firewall 或 namespace helper 事件积压进 Program 结果监控。订阅建立后必须由独立于执行生命周期轮询的读取路径持续排空，不能让其他 Run 的进程事件积压在当前 Program 的 socket 中。实现必须保留内核丢事件或序列缺口的失败证据；如果在 OCI `start` 前无法建立所声明的接收容量和读取路径，必须归入 `ProcessSupervision` 并停止启动该 Program，不能先执行再预告退出结果将为未知。

`create` 成功后，Engine 必须先证明已创建进程的身份、containment 和继续安全控制所需的监督条件，才能尝试 `start`。这项证明或后续监督失败归入 `ProcessSupervision`：已经直接观察到的 `create` 仍为成功，尚未执行的 `start` 仍为未尝试。`Wait` 只表示等待初始进程结果，不承担这项监督错误。

## 标准流与计时

Engine 分别向每个 Program 传递有限的 `stdin`，并分别排空 `stdout` 与 `stderr`。保留的单流输出达到 100 MiB 上限后仍要继续读取，避免子进程阻塞。实际写入量、写端关闭、EOF、读取错误和截断情况都进入输出事实。

执行期限使用单调时钟，从第一个 Program 即将 `start` 时开始，到进入终结阶段时结束。省略期限表示 Program 执行区间不受限制。终结时，全体 Program 共享固定的 10 秒停止宽限期。准备、Runtime 操作、排空、捕获与清理仍使用 Engine 构造时固定的有限内部期限。

## 最终环境

所有相关进程结束或被确认无法继续写入后，Engine 必须移除运行时文件系统。`RunControls.capture_final_environment` 为真时，再从稳定的私有 `rootfs` 构造完整 OCI Image，最终 Image 进入同一套公共验证、Layer 构造和 content-addressed publish 路径；为假时跳过构造和发布，并返回 `not_requested`。

请求捕获而捕获失败时返回明确的不可用原因，并保留已经取得的进程和标准流事实。Engine 不能把临时目录、单独 Layer、Runtime 私有对象、初始 Image 或 `not_requested` 冒充捕获失败的最终环境。关闭捕获不允许跳过 Secret 移除、标准流排空或调用内清理。

## 调用内清理

Engine 负责在一次仍然存活的 `run` 调用内停止并清理自己创建的进程、mount、网络、cgroup、bundle、容器、Secret 交付文件和其他临时对象。清理不能删除调用方引用的宿主资源，也不能回滚可写 bind mount 上的修改。Secret mount 必须在最终环境捕获前移除；未请求捕获时也必须在调用返回前移除。

清理失败进入 `RunOutput`，不能覆盖其他已经取得的事实。析构可以尽力释放局部资源，但不能代替显式的有界终结流程。如果 Engine 进程或宿主机意外终止，这次调用可能没有返回值；本 package 不因此引入持久身份、journal 或恢复接口。

## 具体实现

### `NativeEngine`

`NativeEngine` 只在 Linux 构建，直接实施 OCI Runtime Configuration。Rootful 是 reference profile；rootless 是同一个 Engine 的受限能力 profile。无法忠实实施的字段必须在任何 Program 启动前拒绝。

`NativeEngine` 直接实现 `RunEngine`。`run_engine` 不定义另一个 Backend trait、compatibility Engine 或尚无真实工作流的执行扩展点。

## 验证要求

每个实现都要通过独立进程测试证明：

- 支持的输入字段确实影响执行，不支持的字段在 Program 启动前返回准确的 `EngineError`。
- `stdout`、`stderr`、`stdin`、退出码、信号、超时、取消和创建失败被正确表达。
- Secret 环境变量和文件能够被 Program 读取，Secret 文件不进入最终环境，临时交付资源在调用返回前清理。
- Program 的非零退出等执行结果返回 `RunOutput`，而不是被误分类为 Engine Error。
- 初始环境平台与实际执行平台相容；请求捕获时最终环境可以重新验证和展开，未请求时不发布 OCI 内容并明确返回 `not_requested`。
- 临时资源在一次正常返回的调用内完成有界清理。
- 同一个 Engine 实例及共享同一宿主的不同 Engine 进程并发调用时，网络、进程监控、workspace 和取消状态互相隔离；一条调用的 DNS、返回包或辅助进程事件不能污染另一条调用。

`NativeEngine` 必须在真实 Linux OCI Runtime 环境验证。具体命令、主机组合、测试数量、通过结果和失败记录属于代码仓库的工程证据，不属于本契约。
