# Run Protocol 与 NativeEngine 当前工程状态

本文只记录开发 worktree 的实现事实。稳定产品、协议和架构由 [Agent Wiki](http://localhost:8787/app/pages/runlab-index--nw) 拥有。

## 当前开发边界

当前分支是 `rewrite/native-engine`。本阶段只交付两个 Rust library package：

```text
run_engine -> run_protocol
```

根 package 中继承的 `runlab` binary 仍是 legacy 实现，不是这两个新 package 的目标行为，也没有接入新的 NativeEngine。DockerEngine 已推迟，不属于当前分支的实现或验证范围。

## `run_protocol`

`run_protocol` 只拥有一次执行的输入、输出、错误分类与结构不变量：

- `RunInput`、`ProgramInput`、`RuntimeConfig` 与 OCI `ImageDescriptor`；
- `RunOutput`、逐 Program lifecycle/stdio/process/final-environment 事实；
- `EngineError`、`InputError` 与 `OutputError`；
- exact Runtime Configuration bytes、唯一 primary、完整 Program 映射和 execution ordering 等不变量。

它不拥有 `run_id`、Run Record、持久化、恢复、Catalog、CLI 或 Engine 实现。

输出模型通过私有 facade 保持一个 crate-root API，同时按职责分为 operation、process、stdio、stop 和 aggregate validation；这些物理模块不是新的公共协议层级。

## `run_engine`

公共边界只有同步、阻塞且可并发复用的 `RunEngine::run`、调用级 `CancellationToken`、OCI content store 与有限 operation deadlines。

当前唯一实现是 Linux rootful `NativeEngine`。主要内部边界如下：

| 责任 | 模块 |
| --- | --- |
| invocation orchestration | `native/execution.rs` |
| preflight 与 bundle preparation | `native/prepare.rs`、`native/profile.rs` |
| create/start、wait、stop | `native/start.rs`、`native/wait.rs`、`native/stop.rs` |
| process ownership 与 bounded helpers | `native/subprocess/supervisor.rs`、`native/subprocess/helper.rs` |
| stdio 与 process evidence | `native/stdio.rs`、`native/linux_evidence.rs` |
| runtime/invocation cleanup | `native/cleanup.rs` |
| exact OCI content/Image/Layer | `oci/content.rs`、`oci.rs`、`oci/layer.rs`、`oci/json.rs` |
| rootfs tar 物理预检与共享预算 | `rootfs/preflight.rs` |
| rootfs layer 扫描、计划与安全落盘 | `rootfs/layer.rs`、`rootfs/plan.rs`、`rootfs/apply.rs` |
| stopped capture 与 deterministic layer | `rootfs/capture.rs`、`rootfs/diff.rs`、`rootfs/encode.rs` |
| filesystem safety leaves | `rootfs/digest.rs`、`rootfs/xattr.rs`、`rootfs/mountinfo.rs` |

依赖方向保持为：

```text
execution -> prepare/start/wait/stop/cleanup/capture
prepare/capture -> OCI + rootfs
rootfs layer/capture -> digest/xattr/mountinfo
run_engine -> run_protocol
```

Native lifecycle 函数按 preparation、execution、create/start、wait、signal phase 和 cleanup 分解；生产路径没有 `too_many_lines` 或 `too_many_arguments` lint 例外。rootfs 子模块使用显式依赖，Layer 扫描和 apply 共同依赖独立 plan，不互相反向拥有类型。

没有公共 Backend trait、异步 runtime、恢复接口或 Docker compatibility vocabulary。

## 当前能力限制

- Linux-only、rootful reference profile；rootless 尚未实现。
- `Network::Isolated` 已实现；egress profile 尚未实现。
- 每次最多 8 个 Program，execution timeout 最长 7 天。
- stdout/stderr 各保留固定 100 MiB 原始字节前缀，之后继续排空并记录 omission/EOF。
- capture 依靠 stopped tree 的 two-pass agreement，不是原子 filesystem snapshot。
- OCI Runtime field、mount、device、namespace、capability 与 resource 只接受已证明可忠实执行的子集。
- 不考虑跨调用恢复、journal 或 reconcile。

## 当前验证事实

- macOS：`run_protocol` 20 个测试、`run_engine` 8 个非 Linux 测试通过。
- Linux：`run_protocol` 20 个测试；`run_engine` 83 个测试通过，1 个真实 runc 测试默认显式忽略。
- Linux 完整测试套件曾连续运行 20 轮，用于复核已修正的 executable fixture 竞态。
- macOS 与 Linux Clippy 均以 `-D warnings` 通过；`cargo fmt --check` 与 `git diff --check` 通过。
- macOS 和 Linux 的 Rust 1.95.0 all-target check 通过。
- `run_protocol` package 成功；`run_engine` 在临时 crates.io patch 指向同工作树 `run_protocol` 的条件下成功生成 package archive。真实发布仍必须先发布 `run_protocol 0.1.0`。
- 真实 Lima Linux VM 上以 runc 1.5.1 跑过完整 NativeEngine ignored E2E：1 个测试通过。rootfs-stability 修正后的第一次复跑暴露旧 fixture 把持久输出写入 Engine-owned mount artifact，因而按新安全规则正确拒绝 Final；fixture 分离两类目录后，完整 E2E 重新通过。

以上事实均针对当前未提交工作树。独立 review 结论只对审阅时列明的完整 source hash 有效，本状态文件不替代冻结 manifest 与审阅报告。
