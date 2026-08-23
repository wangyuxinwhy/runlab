# Review Cleanup — 状态

审阅结论见对话；本文件只记录执行状态，便于中断后继续。

## 验证方式

- macOS: `cargo clippy --all-targets --all-features -- -D warnings` + `cargo test --all-targets`
- Linux: `./scripts/linux-check.sh`（docker `rust:1.97.1` 容器）
  约 1/5 的生产代码是 `#[cfg(target_os = "linux")]`，macOS 上从不编译。
- MSRV: `cargo +1.95 check --all-targets --all-features`
- 打包: `cargo package --no-verify`
- 真 Docker 端到端（opt-in）: `RUNLAB_TEST_IMAGE=alpine:3.20 cargo test --test docker_e2e -- --ignored`

当前基线（全部通过）：两平台 clippy 零告警；macOS 140 单测 + 21 契约测试；
Linux 237 单测 + 21 契约测试；MSRV 1.95 通过；打包 76 文件；真 Docker e2e 1 passed / 46.79s。

真 Docker e2e 首次运行失败，原因是本机 `alpine:3.20` 镜像不在了
（`No such image`），不是代码缺陷；`docker image pull` 后重跑通过。两次结果都记录在此。

## 已完成（已提交）

| commit | 内容 |
|---|---|
| `0a55625` | 修红：schema 版本改为 JSON Schema `const`，契约 fixture 从二进制读取；修正一个编码了不可能状态的 native fixture；新增 `scripts/linux-check.sh` |
| `8b02767` | `integrity` 成为持久化私有写入的唯一所有者（`sync_directory` ×7 → 1，私有写入 4 种分叉语义 → 2 个明确原语） |
| `3411114` | 8 个 `native_*` 顶层模块 → 一棵 `native/` 子树，单点 cfg，子树内 71 个 cfg 归零 |
| `237f9b9` | `execution.rs` 2704 行/98 cfg → 934 行/17 cfg；native 执行进 `execution/native.rs`；引入 `RunScope` 接缝 |
| `c2e2285` | 9 个文件的机械切分 import 改为真实 import；5 处 `use super::*` 清零 |
| `602233b` | 三处越界归位：storage 的协议规则 → `TerminalRunRecord::validate`；core 的 native 地址池策略 → `native::network`；image 的 backend preflight → `native::backend`。顺带补上从未校验的 `content_digest` |
| `6321f8b` | `RunControls` 不变量不可绕过；reconcile 的 7 个 status + 35 个 action 从 `&'static str` 变为枚举，出现在公开 schema |
| `ac47bf6` | 15 个自调用隐藏命令的名字统一为常量；guest dispatch 从 `cli.rs` 移入 `cli/vm.rs` |
| `5b7c065` | 30 个模块根全部补模块级文档；`main.rs` 记录实际分层 |
| `7ff6f30` | **flaky 根因修复**：fork 继承的 fd 让已释放的 recovery 锁看起来仍被持有 |
| `8a2d860` | `ingress` / `image_ingress` / `distribution` → 一棵 `ingress/` 树（`local` / `registry` / 根） |
| `17ab200` | `native::network` 从 `io::Result` 改为 `anyhow::Result`；删除第二份 bounded 子进程监管实现（-194 行） |
| `69681c2` | `materialize` 不再在分派后重新匹配 entry kind，6 处无消息 `unreachable!` 消失 |
| `68c1b46` | 其余 `unreachable!` 要么被更窄的类型消除，要么写明理由 |
| `be356f4` | `subprocess` 的 teardown 不再用清理失败盖掉真正的超时/超限原因 |
| `c555189` | `cli.rs` 1275 → 236 行；每个子模块完整拥有一条子命令（参数 + handler + 输出形状） |
| `67e21eb` | 每个 Docker 客户端调用都有超时（控制面 2 分钟 / 搬运字节 30 分钟） |
| `8487676` | `set_nonblocking` / `spawned_child_error` 只保留一份 |

## flaky 测试：已定位并修复（`7ff6f30`）

**根因**：`fork` 复制 fd 表。某个线程 `spawn` 子进程时，子进程继承了本进程所有打开的 fd
——包括 recovery 锁——并让它们的 `flock` 一直有效，直到 `exec` 因 CLOEXEC 关闭它们。
于是「刚 drop 的锁」会在一次无关的 fork/exec 期间被读成 `native recovery attempt is active`。
同一机制也解释了 fake runc 的 `Text file busy`（ETXTBSY）。

**证据**：所有失败都发生在「drop 之后立刻重开」的位置；4 个不同测试在 60 次并行运行中共失败 6 次。
定位手段是给只检查子串的断言补上实际错误文本（该改动本身保留）。

**修复**：`try_lock` / `try_lock_root` 在相信「锁被占用」之前先等过这个窗口（最多 200ms，每 10ms 重试）。
等待不可能从真正的持有者手里抢到锁——持有者会持有整个 attempt——只会阻止已释放的锁被误报为活跃。
fake runc fixture 用同样方式等过 `ETXTBSY`。

**修复后**：60 次并行运行 0 次失败。这是一次真实的下降，不是「不存在」的证明。

## 审阅第七项（错误约定）的复核结论

复核后认为剩余部分**不需要改动**，理由如下，而不是「已完成」：

- `anyhow` 是全仓库默认约定，`native::network` 是唯一的例外，已在 `17ab200` 归位。
- `thiserror`（`RenderError` / `FsPathError` / `PaxError`）用在**调用方确实按变体分支**的地方：
  `render.rs` 把 `PaxError::EntryLimit` 翻译成 `RenderError::LimitExceeded`，
  `materialize` 按 `RenderError::UnresolvedHardlink` 分支。这是类型化错误的正当用途。
- 剩余 `io::Result` 全部是 `impl Read`、`set_nonblocking` 这类系统调用薄封装，
  以及两个**故意**保留原始 `ErrorKind` 的函数（`connect_loopback_tcp`、`process_start_time_ticks`），
  它们的调用方需要区分「还没就绪」和「真的失败」，代码里已写明理由。

## 第二轮审阅（5 项）

外部审阅提出 5 项，逐条对代码核过后**事实全部成立**，但严重度和补救建议有三处调整：

- **#1 pre-acceptance 遗留**：降为「中」。`reconcile_pre_acceptance` + 公开状态 `DiscardedPreAcceptance`
  说明这是被设计过的一等状态，不是无人认领的泄漏；真实问题是单 participant 与 managed 两条路径策略不一致。
  另外，急切清理覆盖不了 SIGKILL，所以 journal + reconcile 这条路本来就必须存在，急切清理是优化不是正确性修复。
- **#2 BackendFacts**：核心问题改为「冗余编码与校验漂移」，不强调防篡改；且指出改 tagged enum 是破坏
  `TerminalRunRecord` schema 7 的变更，因此只收敛校验、不动 schema。
- **#3 CLI coordinator**：撤回「VM transport 难以复用」的论据——`vm exec` 转发的是 argv，guest 里跑的就是同一个 CLI。
  也不引入 `RunApplication`：那是为尚不存在的消费者建层。实际问题是模块文档不诚实。
- **#4 / #5**：维持原判。

| commit | 内容 |
|---|---|
| `cc78d27` | #5 + #1：capture/attach 错误归属参与者自身 scope（同类缺陷共 3 处）；单 participant 未被 accept 时显式丢弃 recovery attempt |
| `28602a8` | #4：`execution/native.rs` 1873 行拆成 `scope` / `participant` / `managed` 三个边界，root 52 行 |
| `16e0bcc` | #2 + #3：共享 backend 不变量收敛到 `BackendFacts::validate`；CLI 模块文档改为诚实的 composition-root 描述 |

### #4 的结构性结果

依赖变成有向无环，且是**声明式的**：

```
managed  ->  participant  ->  scope
```

- `scope` 不依赖 `native` 内的任何东西
- `participant` 只依赖 `scope` 的 `RunScope` 一个类型
- `managed` 从「向父模块伸手拿 27 个私有符号」变成「向两个兄弟模块具名依赖 13 个符号」

`execute_native_primary` 不再解构 `RunScope`；scope 提供一次 `ScopedExecution` 借用，
把「native Run 一定有取消标志和 recovery attempt」这条不变量留在建立它的类型里。

可见性是收窄过的声明面，不是子树内 `pub(super)` 全开：`PreparedNativeBackend` 暴露 4 个字段，
`NativeProcessObservation` 1 个，`NativeExecution` 全部（因为 topology 要为每个 participant 各造一个）。

**注意**：导入数从 27 降到 13 是症状不是判据——它可以用 `use super::*` 或一个 facade struct 绕过。
真正的判据是上面那条有向依赖，以及 root 里已经没有任何逻辑。

### 测试覆盖的诚实边界

- #5 的测试经变异验证：把 `state.scope` 改回 `OperationErrorScope::Primary`，测试失败（`left: Primary, right: ManagedService`）。
- #2 的测试经变异验证：短路掉 name/details 对应检查，测试失败。
- **#1 的覆盖不完整**：新测试钉住的是 `abort_pre_acceptance` 的契约，
  没有钉住 `run_selected` 确实调用了它。要覆盖后者需要一次注入 acceptance 失败的真实 native Run，依赖 runc。

## 待办

无。第一轮 8 项与第二轮 5 项均已处理。
