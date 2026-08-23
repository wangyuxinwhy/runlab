# Review Cleanup — 状态

审阅结论见对话；本文件只记录执行状态，便于中断后继续。

## 验证方式

- macOS: `cargo clippy --all-targets --all-features -- -D warnings` + `cargo test --all-targets`
- Linux: `./scripts/linux-check.sh`（docker `rust:1.97.1` 容器）
  约 1/5 的生产代码是 `#[cfg(target_os = "linux")]`，macOS 上从不编译。
- MSRV: `cargo +1.95 check --all-targets --all-features`
- 打包: `cargo package --no-verify`
- 真 Docker 端到端（opt-in）: `RUNLAB_TEST_IMAGE=alpine:3.20 cargo test --test docker_e2e -- --ignored`

当前基线（全部通过）：两平台 clippy 零告警；macOS 137 单测 + 21 契约测试；
Linux 232 单测 + 21 契约测试；MSRV 1.95 通过；打包 74 文件；真 Docker e2e 1 passed / 49s。

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

## 待办

无。审阅第四节的 8 项已全部处理（第 7 项按上述复核结论定为「无需改动」）。
