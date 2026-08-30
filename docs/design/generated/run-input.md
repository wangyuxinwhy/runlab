---
title: "Run Input"
description: "定义可直接执行的 RunInput，以及 OCI 配置、标准输入、输出限制、超时和网络控制。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# Run Input

`RunInput` 是一份已经解析完成、可以直接交给 `RunEngine::run` 的执行输入。本页定义它包含的内容，以及每个字段对执行意味着什么。

调用方必须在执行前把 Catalog 名称、`tag`、本地目录、远端 `reference` 和产品默认值解析为下文定义的确切对象，再构造 `RunInput`。

## 结构

```text
RunInput
├── programs
│   ├── primary
│   │   ├── initial_environment
│   │   ├── runtime_config
│   │   ├── stdin
│   │   └── secrets
│   │       ├── env
│   │       └── files
│   └── ...
└── controls
    ├── execution_timeout_ms（可选）
    ├── network
    └── capture_final_environment
```

`programs` 是以 `program_id` 为键的映射。它必须包含键 `primary`，还可以包含零个或多个其他 Program。映射顺序没有语义，同一输入中的键必须唯一。

`controls` 是作用于整次协议调用的 `RunControls`。它只包含执行引擎必须实施的控制：完整执行期限、跨边界网络策略，以及是否捕获每个 Program 的最终环境。这些控制不属于某个 Program 的 OCI Runtime Configuration，也不包含产品身份、持久化或恢复语义。

## Program 输入

每个 Program 包含四项输入：

- `initial_environment`：指向 OCI Image Manifest 的 Descriptor，描述启动前的受控文件系统。
- `runtime_config`：完整的 Linux OCI Runtime Configuration，描述进程和运行条件。
- `stdin`：启动后按顺序传给 Program 的、长度有限的原始字节序列。
- `secrets`：只在执行期间交付给 Program 的敏感环境变量和只读文件。

每个 Program 从自己的初始环境展开出独立的可写 `rootfs`。不同 Program 即使引用同一个 Image，也不能共享这份可写状态。

## `initial_environment`

`initial_environment` 必须是经过验证、内容可取得的 OCI Image Descriptor，明确包含 `mediaType`、`digest` 和 `size`。它必须指向符合 [OCI Image Specification 1.1.1](https://github.com/opencontainers/image-spec/tree/v1.1.1) 的 Linux Image Manifest。Manifest、Image Config 和全部 Layers 必须能够通过 Descriptor 逐项验证。

Descriptor 标识确切的 OCI Image 内容。Catalog 名称、`tag` 和远端 `reference` 可能改变解析结果，因此只能用于协议外构造 `RunInput`，不能代替 Descriptor。

## `runtime_config`

每个 Program 的 `runtime_config` 必须符合 [OCI Runtime Specification 1.3.0](https://github.com/opencontainers/runtime-spec/blob/v1.3.0/config.md)，并且 `ociVersion` 明确为 `1.3.0`。

它定义进程参数、环境变量、用户、工作目录、资源约束、`mount`、`namespace` 和其他运行条件。Image Config 中的默认入口或环境变量不能隐式补入。调用方若要使用它们，必须在构造 `RunInput` 时得到一份已经合并完成的 `runtime_config`。

OCI 字段、默认值和 `options` 按相应标准解释。Run Protocol 不复制这些细则，也不改写通过验证的配置来制造统一外观。

### 初始环境与 `root.path`

`root.path` 必须使用相对于 OCI bundle 的字面值 `rootfs`：

```json
{
  "root": {
    "path": "rootfs"
  }
}
```

`rootfs` 是 `initial_environment` 提供的根文件系统槽位。直接调用 OCI Runtime 的实现会把它物化到 `<bundle>/rootfs`。采用其他执行机制的实现必须忠实提供相同的根文件系统语义。实现生成的绝对路径不写回 `RunInput`。`root` 的其他字段按 OCI 语义处理。

### 调用方声明的宿主资源

`runtime_config` 可以通过 `mount`、`hook`、已有 `namespace`、`cgroup` 或其他 OCI 字段引用宿主资源。显式提供的字段和值属于执行输入，必须原样交给 Runtime。引擎不得把它们替换为自己生成的临时路径。

相同的宿主路径只表示两次输入引用了相同位置，不能证明该位置的内容或行为相同。实验方负责判断这些外部状态是否满足自己的对照条件。引擎只承诺忠实实施配置，并记录执行时直接观察到的结果。

`bind mount` 的字段和访问方式完全由 OCI `mount` 配置表达。Run Protocol 不计算挂载内容的 `digest`，不管理它的版本，也不捕获挂载后的外部文件系统状态。无论 `mount` 可读还是可写，它都不进入 Program 的初始或最终环境。

如果某个字段违反 OCI 标准、与本协议的标准流或网络语义冲突，或者引擎无法忠实执行该字段，引擎必须在启动任何 Program 前返回 `EngineError`。例如，本协议要求 `stdin`、`stdout` 和 `stderr` 分离，因此 `process.terminal` 必须为 `false`。

## Secrets

`secrets` 包含两个映射：

- `env` 以环境变量名为键，以需要交付的精确字节为值。变量名必须符合 `[A-Za-z_][A-Za-z0-9_]*`，值必须是 UTF-8 且不能包含 NUL。
- `files` 以 Program 内的绝对路径为键，以需要交付的精确字节为值。目标必须是规范化的绝对 Linux 路径，不能是 `/`。文件内容可以是任意字节。

映射键必须唯一，顺序没有语义。空映射表示没有对应类型的 Secret。

Secret 属于完整 `RunInput`，但不属于 OCI Runtime Configuration。它表达的是“把哪些敏感字节交付到哪里”，不表达调用方从环境变量、宿主文件、Keychain、Credential Cache 或其他来源取得这些字节的过程。来源解析属于调用方；交给 Engine 的已经是确切值。

Engine 必须把 `env` 值作为对应 Program 的进程环境交付，把 `files` 值作为仅在执行期间存在的只读常规文件交付。Secret 环境变量名不能与 `runtime_config.process.env` 中已有名称重复；Secret 文件目标不能与 `runtime_config.mounts` 中已有目标重复。冲突是无歧义规则，不表示 Secret 可以覆盖 Runtime Configuration。

实现可以为交付 Secret 派生仅在调用内使用的 bundle 配置和临时文件，但不能修改 `RunInput`，也不能把派生配置冒充调用方提供的 `runtime_config`。Secret 文件不是初始环境的一部分，执行结束后必须先从 `rootfs` 移除，不能进入最终环境。

这项边界只防止交付机制隐式保存 Secret。Program 已经能够读取 Secret，因此也能够主动把它复制到自己的可写文件系统或写入 `stdout`、`stderr`；这些字节随后按普通执行结果处理。协议和 Engine 不猜测哪些 Program 输出包含 Secret。

Run Protocol 规定执行语义，不规定持久化策略。采用协议的产品若保存 Run Record，必须另外定义 Secret 是否保存以及如何表示；不能因为某个产品选择不保存 Secret 值，就把 Secret 排除在 `RunInput` 之外。

## 标准输入

`stdin` 是有限的原始字节序列。每个 Program 最多 10 MiB，即 10,485,760 字节。长度按原始字节计算。省略时等同于空字节序列。

引擎必须从第一个字节开始，按顺序传递 `stdin`。传完所有字节后，引擎必须成功关闭输入写端，使 EOF 可供 Program 读取。协议只能证明字节写入和写端关闭，不能证明 Program 实际执行了读取。Program 提前关闭输入或写入失败时，引擎停止继续写入，并在输出中记录实际写入字节数和错误。`run` 调用开始后不能再向这段输入追加字节。

## 输出保留限制

每个 Program 的 `stdout` 和 `stderr` 分别最多保留前 100 MiB，即 104,857,600 字节。这个限制是引擎的固定行为，不是 `RunInput` 字段。

达到上限后，引擎仍须排空对应流，避免 Program 因管道阻塞，并在 `RunOutput` 中记录是否确实省略了后续字节。两个流保持独立。协议不重建它们之间的全局时间顺序。

## 执行超时

`execution_timeout_ms` 是可选正整数，表示整个执行区间的最长持续时间。省略表示没有执行期限，引擎不得补充隐藏的默认超时。

执行区间从第一个 Program 即将执行 OCI `start` 时开始，到执行进入终结阶段时停止。期限到达后，引擎停止启动新的 Program，并按照 [Run Engine](/design/generated/run-engine) 中规定的有界流程停止已经启动的 Program。

实现可以声明自身支持的有限超时范围。显式数值超出范围时，必须在启动任何 Program 前返回 `EngineError`，不能截断或替换。

## 网络控制

`network` 有两个值：

| 值 | 含义 |
| --- | --- |
| `isolated` | 阻止跨越本次执行边界的入站和出站网络流量。 |
| `egress` | 允许 Program 主动建立到执行边界之外的连接，也允许该连接的返回流量，同时阻止外部主动建立新的入站连接。 |

`network` 必须在 `RunInput` 中明确提供。请求构造层可以采用自己的默认值，但交给引擎的输入不能依赖该默认值。

网络控制只约束是否允许流量跨越本次执行边界，不规定 Program 之间的网络拓扑。每个 Program 是否共享 `network namespace`、能否通过 `127.0.0.1` 相互访问、使用什么地址和端口，均由 OCI Runtime Configuration 与引擎明确支持的机制决定。`127.0.0.1` 只指向相应进程所在的 `network namespace`。协议不保证所有 Program 共享 `loopback`。

`egress` 是许可，而不是连通性保证。协议不保证 DNS 配置、具体路由、地址分配、远端可用性或返回内容。使用外部 DNS 本身属于 `egress`。在 `isolated` 下，引擎不能通过宿主 DNS 绕过隔离。OCI 配置若加入宿主 `network namespace` 或其他跨边界路径，必须与 `network` 一同验证。与所选控制冲突时返回 `EngineError`。

## 最终环境捕获

`capture_final_environment` 是必填布尔值，作用于本次调用中的全部 Program：

| 值 | 含义 |
| --- | --- |
| `true` | 进程停止并且运行时文件系统移除后，从每个稳定的受控 `rootfs` 构造并发布完整 OCI Image。 |
| `false` | 不构造或发布最终 OCI Image；`RunOutput` 对每个 Program 明确返回 `not_requested`。 |

关闭捕获只省略最终环境的构造和发布，不改变执行、标准流排空、进程结果、停止流程、Secret 移除或临时资源清理。它不是 dry run，也不回滚 Program 对外部资源造成的副作用。所有 Program 共用同一个值，协议不支持在一次调用内只捕获部分 Program。

## 输入比较

比较两份 `RunInput` 时，按照以下规则比较它们的结构和值：

| 输入部分 | 比较规则 |
| --- | --- |
| Program | 按 `program_id` 对应。 |
| `stdin` | 按原始字节比较。 |
| `secrets.env` | 按变量名和原始值对应比较。 |
| `secrets.files` | 按目标路径和原始内容对应比较。 |
| `initial_environment` | 按完整 Descriptor 比较。 |
| OCI JSON | 对象的成员顺序没有语义，数组顺序保留语义。 |
| `controls` | 按每个显式字段的值比较。 |

协议不尝试证明两个不同的 OCI 配置具有相同行为，也不尝试证明相同宿主路径在两次调用中具有相同外部状态。
