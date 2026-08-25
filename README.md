# RunLab

RunLab 是建立在 OCI 之上的 Run 执行系统。它负责一条 Run 从创建、接受、执行到终结的完整生命周期，并将执行输入、执行事实和执行结果保存为不可变 Run Record。

RunLab 不理解程序内部流程，也不声明记录非受控外部环境。程序可以自行把日志、Trace 和中间产物写入 stdout、stderr 或受控文件系统；RunLab 忠实保存这些内容与 Final OCI Image，但不解释其领域语义。Agent Loop 是典型场景，不是核心协议。

RunLab 不定义 Experiment、评分或编排 DSL。稳定设计以 [RunLab 文档索引](http://localhost:8787/app/pages/runlab-index--nw) 为入口；Run 的概念推导见 [Agent Loop 实验的本质](http://localhost:8787/app/pages/execution-to-agent-loop-asset--md)；当前代码事实见 [RunLab 当前实现](http://localhost:8787/app/pages/runlab-current-implementation--hv)。本仓库中的 [IMPLEMENTATION.md](IMPLEMENTATION.md) 和 [ROADMAP.md](ROADMAP.md) 只保存开发所需的当前状态与剩余门禁。

## Linux 快速开始

reference path 需要 Linux、Rust 1.95+ 和 runc 1.5.1。rootful profile 还依赖 cgroup v2 与 OverlayFS；普通用户可使用能力更窄的 rootless profile。

```bash
cargo build --release --locked
runlab=./target/release/runlab

$runlab --state ./state image pull \
  registry-1.docker.io/library/alpine:3.22 \
  --name alpine-hello

$runlab --state ./state runtime-config create \
  alpine-hello \
  --output config.json

jq '.process.args = ["/bin/sh", "-c", "printf \"Hello RunLab\\n\""]' \
  config.json > hello-config.json

$runlab runtime-config check hello-config.json
$runlab --state ./state run start \
  alpine-hello \
  --runtime-config hello-config.json
```

`run start` 输出的 JSON 包含 `run_id`。使用同一 State 读取记录和精确 stream bytes：

```bash
$runlab --state ./state run get <run-id>
$runlab --state ./state run stdout get <run-id> --output stdout.bin
$runlab --state ./state run verify <run-id>
```

完整教程和 profile 前提见 [在 Linux 上运行第一个 Run](http://localhost:8787/app/pages/hello-runlab-linux--bn)。

## macOS

macOS 不直接执行 Linux OCI bundle。`runlab vm` 管理一个无 host mount 的本地 Linux VM，State、rootfs、runc 与 Final Image 都留在 guest disk。

```bash
runlab vm create --instance runlab
runlab vm start --instance runlab
runlab vm install --instance runlab \
  --binary /path/to/linux-runlab \
  --runc /path/to/runc-1.5.1
runlab vm status --instance runlab
```

后续命令通过 `vm exec --namespace <name>` 和显式 `@input/N`、`@output/N` file slots 运行。完整流程见 [在 macOS 上运行第一个 Run](http://localhost:8787/app/pages/hello-runlab-macos--pp)。

## State 维护

`--state` 选择本机 OCI Store、Catalog、Run Database 与 recovery state。普通读取不隐式修复或删除内容。

```bash
runlab --state ./state state verify
runlab --state ./state run reconcile <run-id> --dry-run
runlab --state ./state state gc plan --output gc-plan.json
runlab --state ./state state gc apply gc-plan.json
```

恢复和垃圾回收语义见 [检查、恢复与维护 RunLab State](http://localhost:8787/app/pages/maintain-runlab-state--pz)。

## 开发验证

```bash
cargo fmt --check
cargo test --all-targets --locked
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo +1.95.0 check --all-targets --all-features --locked
cargo package --no-verify --locked
```

真实 Native、Rootless、Docker 和 managed VM 验证需要各自声明的环境。当前证据及其不能支持的结论见 [RunLab 当前验证矩阵](http://localhost:8787/app/pages/runlab-verification-matrix--vy)。
