---
title: "RunLab 软件架构"
description: "定义 RunLab 的长期模块责任、依赖方向、Image 与执行数据路径及抽象准入规则。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# RunLab 软件架构

本页定义 RunLab 的长期代码责任和依赖方向，不枚举当前文件树、函数路径或验证结果。

## 一个产品表面，三个 Rust package

RunLab 维护一个 `runlab` 产品表面，并以三个 Rust 2024 package 实现：

```text
runlab
  ├── run_engine
  │     └── run_protocol
  └── run_protocol
```

- `run_protocol` 是纯协议 library crate。
- `run_engine` 是协议执行 library crate。
- `runlab` 是唯一的产品 binary crate。

拆分 package 不产生第二套协议、兼容产品或额外 CLI。CLI、请求与结果 JSON、OCI 内容的原始字节，以及持久化的 Run Asset 仍由一个 `runlab` 产品表面组织。

实现使用阻塞式进程、文件系统和线程原语。OCI Runtime 通过范围明确的子进程边界调用。代码禁止 `unsafe`。经过验证的 OCI 原始字节必须保留，不能用解析后的 JSON 对象重新序列化并替换。

## Package 责任

| Package | 负责 | 不负责 |
| --- | --- | --- |
| `run_protocol` | `RunInput`、`RunOutput`、`EngineError`、OCI 协议值和纯结构不变量 | Engine 实现、取消控制、内容存储、`run_id`、Run Record、Catalog、CLI |
| `run_engine` | Rust `RunEngine` 接口、调用级取消、OCI 内容访问边界、Image 执行管线与 `NativeEngine` | Run identity、Run Record、数据库事务、Catalog 名称、CLI、Experiment |
| `runlab` | 请求构造、Catalog、Run identity、Run Record、Storage、公共查询面、Coordinator、CLI 和 macOS VM transport | 改写协议执行语义或维护另一套 Engine 模型 |

`run_protocol` 的公共类型不依赖某种传输、存储或执行机制。`run_engine` 接收已经解析完成的 `RunInput`，不解析名称、`tag` 或产品默认值。`runlab` 在调用 Engine 前完成产品请求到协议输入的构造，并在调用外管理持久身份与结果发布。

实际 package 内部可以继续按 `integrity`、`oci`、`image`、`native`、`storage`、`query`、`coordinator` 和 `cli` 等责任组织私有模块，但不能让一个私有模块反转 package 依赖或取得不属于自己的事实所有权。

## 依赖方向

稳定方向是：

```text
runlab → run_engine → run_protocol
   └────────────────→ run_protocol
```

`run_protocol` 不依赖另外两个 package。`run_engine` 不依赖 `runlab`，也不读取 Run Database。`runlab` 可以把自己的 OCI content-addressed store 通过窄接口注入 Engine，但 Engine 不能借此取得 Catalog、路径命名或持久 Run 生命周期能力。

## Image 数据路径

经过验证的原始字节与解析后的类型视图必须分开保留。Engine 对初始环境的验证、展开和最终环境构造进入同一套逐字节安全路径：

```text
verified Initial Image
→ private writable rootfs
→ stopped filesystem state
→ byte-safe ChangeSet
→ deterministic Layer
→ Final Config + Manifest
→ content-addressed publish
```

`OciContentStore` 只提供按完整 Descriptor 读取和发布确切内容的能力。Catalog 名称解析、远端 ingress、引用保留和垃圾回收属于 `runlab`，不进入 Engine 接口。

## 执行数据路径

```text
RunLab request
→ resolve selectors and product defaults
→ RunInput
→ RunEngine::run
→ Result<RunOutput, EngineError>
→ associate with RunLab identity and publish
```

`RunEngine::run` 自己验证输入与当前能力。调用级取消是 Rust 执行接口的旁路控制，不写入 `RunInput`。受控依赖程序由同一个 Engine 协调，使用与 `primary` 相同的 Program 输入和输出模型。

macOS host transport 不实现第三种 Engine。它把操作交给 Linux VM 中的同版本 `runlab`，由虚拟机内的 `NativeEngine` 执行。

## 产品查询路径

Run 的查询能力只存在于 `runlab` package：Storage 保存内部表示，公共 schema 把稳定产品事实投影成 Relation，Query Plane 对一条只读 SQL statement 实施行、cell、总输出和时间边界，CLI 只解析输入并返回结构化结果。`run_protocol` 和 `run_engine` 不知道 SQL、Relation、Run metadata 或 Catalog 名称。

公共 Relation 与私有 SQLite schema 是两个边界。调用方只能依赖可由 CLI schema 命令发现的公共名称、列和语义；内部表、索引和持久 JSON 路径可以随实现迁移。`run list` 保持为小型最近视图，复杂选择与聚合进入公共 Query Plane，完整单 Run 事实仍由 `run get` 返回。

## 抽象准入

`NativeEngine` 直接实现 `RunEngine` 接口。当前不预设额外的通用 Backend trait、compatibility Engine、ORM、Repository、异步 Runtime 或 SDK 包装服务。

具体 Engine 的能力限制不能反向缩窄 Run Protocol。新的执行抽象只能由已经存在的真实工作流和重复机制证明，不能作为未来占位预先进入架构。

注释只解释不容易从代码本身看出的不变量、安全边界、外部规范约束或拒绝原因。名称和类型应直接表达普通控制流。

## 验证边界

工程验证至少覆盖格式检查、所有 target 的测试、将警告视为错误的 Clippy、声明的 MSRV、各 package 打包、独立进程契约测试和真实 Engine 路径。

`run_protocol` 必须可以独立构建和验证。`run_engine` 必须在真实 Linux OCI Runtime 环境验证 Native 路径。未运行的环境测试不得报告为通过。具体命令、测试计数、主机版本和当前结果由代码仓库拥有。
