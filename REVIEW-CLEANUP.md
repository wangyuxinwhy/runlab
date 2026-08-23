# Review Cleanup — 状态

审阅结论见对话；本文件只记录执行状态，便于中断后继续。

## 验证方式

- macOS: `cargo clippy --all-targets` + `cargo test --all-targets`
- Linux: `./scripts/linux-check.sh`（docker `rust:1.97.1` 容器）
  约 1/5 的生产代码是 `#[cfg(target_os = "linux")]`，macOS 上从不编译。

当前基线：两平台 clippy 零告警；macOS 137 单测 + 21 契约测试；Linux 232 单测 + 21 契约测试。

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

## 进行中：`native::recovery::tests` 存在低频 flaky

**这是一个真实缺陷，不是重跑就能消除的。**

证据：
- 并发（默认）跑 `cargo test --bin runlab`：25 次中第 8 次失败。
  失败样本 1：`resolver_fact_mismatches_fail_closed_before_source_reopen`
  失败样本 2：`rejects_truncated_unknown_and_oversized_journals`
  两者都是「断言期望错误消息 A，实际得到 B」。
- `--test-threads=1`：20 次全绿。

→ 与并发相关。两个测试都用各自的 `tempfile::tempdir()`，未发现显式共享状态；
   下一步需要定位是哪个进程级资源（候选：`flock` 语义、signal-hook 注册、
   `/run/runlab` 主机锁路径、或 `read_journal` 中依赖顺序的校验分支）。

在定位清楚之前，**不要把这些测试的断言改成匹配当前行为**——那会把缺陷钉成契约。

## 待办（按审阅第四节顺序，剩余项）

1. **flaky 测试定位与修复**（上文）。
2. `ingress` / `image_ingress` / `distribution` 合并为一棵 `ingress/` 树，消除命名混淆。
3. `subprocess` 抽象只有一个调用者：`native::network::run_bounded_command` 与它是同一件事，
   仅错误类型不同（`io::Result` vs `anyhow::Result`）。
4. `materialize.rs` 中 6 处无消息 `unreachable!` —— `LayerEntryKind` 对该路径过宽，
   需要一个更窄的「已解析条目」类型。全仓库仍有 ~24 处 `unreachable!`。
5. `docker::invoke` 无超时（用户已要求 Docker 优先级后置）。
6. `subprocess::bounded_output` 三处相同 teardown 序列的 `?` / `let _` 纪律不一致。
7. 错误约定三套并存（`anyhow` / `thiserror` / `io::Result`），同层模块选择不一致。
8. `cli.rs` 仍持有 ~30 个输出 DTO，与其 handler 分处两地。
