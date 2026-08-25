# RunLab 当前工程状态

本文只记录开发 worktree 的实现事实。稳定产品、协议和架构由 [Agent Wiki](http://localhost:8787/app/pages/runlab-index--nw) 拥有；更完整的当前快照和证据分别见 [RunLab 当前实现](http://localhost:8787/app/pages/runlab-current-implementation--hv) 与 [当前验证矩阵](http://localhost:8787/app/pages/runlab-verification-matrix--vy)。

## 基线

```text
repository: /Users/bytedance/workspace/temp/runlab-protocol
branch: rewrite/run-protocol
HEAD: b3cd5f547d5d41f584979293dd27265ed382bedf
package: runlab 0.2.0-dev.0
edition: Rust 2024
toolchain: 1.97.1
MSRV: 1.95
```

当前内容描述迁移前 checkpoint 的实现状态。上面的 commit 是这组工作开始时的父提交；本文件随代码、实验记录和验证结果一起进入 checkpoint，后续实现事实以新的 checkpoint commit 为准。

## 当前表面

只有一个 Rust binary crate 和一个 `runlab` CLI：

```text
runlab
├── image
├── docker
├── runtime-config
├── managed-service
├── run
├── state
├── vm
└── schema
```

native Linux 是 `run start` 默认路径；Docker 是显式 compatibility adapter。成功 result 写 stdout JSON，diagnostic 写 stderr。当前 `schema list` 暴露 36 个 versioned success-result schema；error 仍是 stderr contract，不是 JSON schema。

## 实际模块入口

| 责任 | 当前模块 |
| --- | --- |
| CLI composition | `src/main.rs`、`src/cli.rs`、`src/cli/` |
| Run vocabulary | `src/core.rs`、`src/topology.rs` |
| exact bytes 与 OCI | `src/integrity.rs`、`src/oci.rs` |
| ingress 与 Catalog | `src/ingress/`、`src/catalog.rs` |
| Image render/capture | `src/image.rs`、`src/render.rs`、`src/filesystem/`、`src/changeset/` |
| Runtime 与 bundle | `src/runtime.rs`、`src/bundle.rs` |
| Run coordination | `src/execution.rs`、`src/execution/native/` |
| Native mechanics | `src/native/`、`src/native.rs` |
| Docker adapter | `src/docker/` |
| persistence/maintenance | `src/storage.rs`、`src/state.rs`、`src/maintenance.rs` |
| macOS transport | `src/managed_vm/` |

`src/execution/native/` 的依赖方向是 `managed -> participant -> scope`。`RunScope` 持有 cancellation 与 recovery attempt；participant path 负责单个 OCI participant；managed path 只组合两个 participant 的有界 lifecycle。

## 已接通的纵切

- Docker-free OCI Layout/archive import、Distribution pull、Catalog 与 Image read plane；
- deterministic changeset、OCI Layer 和 Final Image assembly；
- accepted/terminal Run Record、exact Runtime Config/stdin/streams 与 SQLite immutable boundary；
- rootful Native execution：verified rootfs、OverlayFS、runc、cgroup、network、capture 与 recovery；
- restricted rootless execution：single-ID、单 participant、`network=none`、direct writable rootfs；
- 一个 required Managed Service：shared loopback、TCP readiness、独立 facts 和 Final Images；
- State verify、Run verify、显式 reconcile 与两阶段 GC；
- Lima managed VM 的 versioned host/guest protocol 和 recoverable operation。

## 当前限制

- Registry credentials、retry、push、referrers 与 signature verification 未完成；
- capture 依靠 two-pass agreement，不是原子 filesystem snapshot；OverlayFS upperdir decoder 未进入 production；
- OCI Runtime field、resource、mount 与 Linux distribution/kernel matrix 仍是受限 profile；
- Docker default mounts、namespace、安全与 daemon policy 尚无完整 fidelity 证明；
- Docker stop/wait helper 仍缺独立 wall-clock 上限；
- managed VM 的 transport-loss、disk-full、upgrade、自动 artifact 与长期自有 VM image gates 未闭合；
- JSON error schema、Secret provider 和自动 redaction 不存在。

Experiment、Matrix、评分和跨 Run 编排是产品非目标，不属于这份缺口清单。

## 验证入口

本机门禁：

```bash
cargo fmt --check
cargo check --all-targets --all-features --locked
cargo test --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo +1.95.0 check --all-targets --all-features --locked
cargo package --no-verify --locked
```

环境相关测试位于 `tests/native_e2e.rs`、`tests/rootless_e2e.rs` 和 `tests/docker_e2e.rs`，必须显式提供所需 runtime、权限或 Image。不能把 ignored test 当作已经通过。
