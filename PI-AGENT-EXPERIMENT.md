# Pi Agent Skill 消融端到端实验

本文件记录 2026-08-23 在 macOS arm64 上通过当前 RunLab managed VM reference path 执行的一次真实 Pi Agent 实验。它是实现证据和临时工程记录，不是 Run Protocol 的第二份规范。

## 结论

端到端路径在修复两个 managed VM 实现缺陷后跑通：同一个 Pi Image、模型、Prompt、工具集和 Run Controls 下，no-Skill arm 没有完成任务，with-Skill arm 生成了 verifier 期望的 exact bytes。RunLab 正确保存了两臂的 Runtime Config identity、process outcome、exact/partial streams、Final OCI Image、backend realization 和 cleanup facts，且 Run/state verification 全部通过。

这证明当前 Run Protocol 足以作为 Agent 实验的单次执行与事实资产层，但不能证明当前产品已经适合开箱即用地编排实验。第一次运行仍需要调用方自行安装并配置 Lima、构建同版本 Linux RunLab、取得精确 runc、构建 Agent OCI Image、生成两份完整 Runtime Config、把 provider credential 转成只读文件 mount、逐次运行、提取 Final Image 文件并解释 `run diff`。RunLab Core 有意不定义 Experiment、Matrix 或 judgment；当前也没有便利的上层 authoring/runner 补足这些工作。

后续从已经运行的 VM、已安装 engine、已导入 Image 和既有 Runtime Config 出发，又完成了一轮 Prompt 精细度与工具集复测。该复测修正了前一轮对易用性的部分判断：首次准备成本不能代表日常实验循环；标准 OCI JSON 的机械修改、现有数字 file slot 和原子 `vm exec` 在本轮没有造成操作错误，因而没有证据支持为它们增加 RunLab 语法糖。warmed-state 中更明确的摩擦是命令身份冲突、错误因果可能被 output publication 遮蔽，以及单次短 Agent 进程前后的 Run 阶段耗时。

本实验每个有效 arm 只运行一次，不能据此宣称 Skill 对一般任务有统计意义上的优势。它只支持当前冻结 task 上的 E2E 可行性和产品摩擦结论。

## 冻结输入

- Source commit：`b3cd5f547d5d41f584979293dd27265ed382bedf`；实验期间另外修复了下述两个未提交 managed VM 缺陷。
- Host RunLab binary：0.2.0-dev.0，SHA-256 `aa218088e18bef0e5ab48f06b21b22f8b6df173f36ca78e25df4c3ea268116c2`。
- Guest RunLab binary：0.2.0-dev.0，Linux arm64，SHA-256 `ead95f207328642157d7e18ca49b0cea89420853f90d861c228f8fd21265ec3d`。
- Lima：2.2.0，VZ、plain、同架构、0 host mounts；固定 Ubuntu 24.04 Image digest `sha256:7df0201546f75b8bcc1044594c806c35749421ad3c9bc1be2a3ab806cfae39cc`。
- runc：1.5.1，commit `v1.5.1-0-g8f2685a47`，Runtime Spec 1.3.0，artifact SHA-256 `ca70e7dbd6616ca782a59b5d3ac86909123fdaa9fa3f89dcf29051c70eee7ce9`。
- Pi：`@earendil-works/pi-coding-agent` 0.84.2；npm integrity `sha512-l4E+B7hgXKWddRo8bC/eSue2aWZjEgJ9xIpf5p0Og+lq8a2TArCwJ0HCoCPCgaBP/tN4zbYH/wOwvx9pJpeLCA==`。
- Base Image：`node:24.3.0-bookworm-slim` index digest `sha256:8225b1806c6e37dced949224b5c0d8278a2fe593967288620e0af69b2cbc4539`，selected Linux/arm64 Manifest `sha256:5020457c330b53d20d6c03b77f888adb767fdd1d9209cdee2462a65d1a392eca`。
- Experiment OCI archive：SHA-256 `d170500595da518552c330b51d8cec842118ea246cdf87a651658e1fc1943e4c`；RunLab selected Manifest `sha256:7042ff155aba55ca49113a190ba0d153ca7918777376b3839cc2a5aeb316c345`。
- Task bytes SHA-256：`f7791c4931a60d1d822947831334163454d5968b19d87f71d8171ebad1895d59`。
- Skill bytes SHA-256：`7257489db9a9bd492cd855a961eadef3312b98ba51d4168e50f079310c2697ea`。
- Model：`deepseek/deepseek-v4-flash`；thinking `low`。
- Built-in tools：`read,write`。
- Prompt：`Read /workspace/task.txt and complete it. Do not explain the task; use the write tool to create the requested output file.`
- Controls：timeout 180 seconds，stdout/stderr limit 各 1,048,576 bytes，network `egress`，empty stdin。
- Verifier：Final Image 的 `/workspace/result.txt` 必须精确等于 `RL::AMBER::52\n`。
- Credential：由 host `DEEPSEEK_API_KEY` 在私有临时目录生成 provider-specific `auth.json`，作为结构化 Runtime Config input seal 成 guest root-owned 0600 read-only file。值、digest 和 bytes 不写入本文件。

两臂的 Runtime Config 源文件逐字 diff 只有 Pi Skill argv：

- Arm A：`--no-skills`
- Arm B：`--skill /opt/runlab-skills/protocol-codec/SKILL.md`

其余 argv、OCI fields、mount、Image 和 Controls 相同。RunLab acceptance 后保存的 canonical Runtime Config digest 分别为 `sha256:62daa16a89e31cb281d1087edb97957110314020e980d36c129c40ec7c9a6da2` 和 `sha256:7b71772716b53bc7eecf7524a933e33dbcdcbe336fc76857b478db2c2b92b0c1`。

## 有效实验结果

### Arm A：no Skill

- Run ID：`run-01a02d29-71d9-7a23-afab-f87af09253ce`。
- Process：`capture_limit_exceeded`，exit 137，非 OOM；约 32.89 秒。
- stdout：partial，精确保存 1,048,576-byte prefix；stderr：available、0 bytes。
- Pi trace prefix 中有 17 次 `read`，没有 `write`。Agent 读取 task 后尝试查找 `/workspace/README.md`、`protocol.md`、`AGENTS.md`、Pi docs 等协议来源，未取得解码规则。
- Final Manifest：`sha256:af3eb752d49c4e3284ab93afb7f594ef0c6c36146893572f70e74aa13cc12a3d`。
- Verifier：失败；Final Image 不存在 `/workspace/result.txt`。
- `run verify`：valid。

### Arm B：with Skill

- Run ID：`run-01a02d2b-1b8a-7842-882a-682f27d2704e`。
- Process：`process_exited`，exit 0，非 OOM；约 5.27 秒。
- stdout：available、52,170 bytes；stderr：available、0 bytes。
- Pi trace 中有 2 次 `read` 和 1 次 `write`：读取 task、读取显式 Skill、写入 result。
- Final Manifest：`sha256:501271cba3518e6d7d7496cb13031d49fd3ac4e51c4c527cd589f599291064d0`。
- Verifier：通过；`/workspace/result.txt` 是 14 bytes，digest `sha256:ba9463bfbbf0548ceb8ba23490853cc706d6e2585d248e74e0fc1f39fb59e901`，exact bytes 为 `RL::AMBER::52\n`。
- `run verify`：valid。

Final Image 中 `/root/.pi/agent/auth.json` 仍为 Initial Image 的 0-byte ordinary file，digest `sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`，证明本次只读 credential projection 在 capture 前被移除，没有进入 Final Image。

末态 `state verify` 为 valid：3 条 terminal Run（包括下述无效 credential preflight Run），0 accepted Run、0 orphan blob、0 staging entry、0 recovery entry。

## 已准备状态复测：Prompt 精细度与工具集

本轮不把 Lima、VM、guest binary、runc、Agent Image 或基础 task/Skill 的首次准备计入易用性评价。复用正在运行的 `runlab` VM、`pi-skill-e2e` namespace、Image `pi-skill-e2e:latest` 和上一轮成功 Runtime Config，只对标准 OCI Runtime Config 的 `process.args` 做机械修改。三臂在看到结果前冻结并各运行一次：

- P-detailed：原 Prompt，工具 `read,write`；
- P-terse：Prompt 缩短为 `Complete the task in /workspace/task.txt.`，工具仍为 `read,write`；
- T-extra：原 Prompt，工具改为 `read,write,bash`。

三份源 Runtime Config 均通过 `runlab runtime-config check`。源文件 SHA-256 分别为 P-detailed `088655ea68a117ff22c6ab4b14cb504a09ee7a8f599e706f4608cb5f33229d05`、P-terse `b2f8db54dd886bed9283013c010391ca0bcb8c47fc099bd170d21d6b8a16e097`、T-extra `363aef31a4cc3f8fda1eebf063671ed5966e9034c6297a1c44d45d142667b00f`。acceptance 后保存的 Runtime Config digest 分别为 `sha256:3db2a07310eadbfc8b81a8b1e897776ec5c82af528776acc32c670be651d33ee`、`sha256:fb7fc5bb1f32ec3e639fe241e4a0c1166832069da6d056893dbec70b0ea816d1` 和 `sha256:648b46e3119ff9c52c8e6bcb8e4dcd58bb503dcc213c548e9a6c744108390523`。source 与 accepted identity 不相同是因为 VM transport 在 guest 中把明确标记的 Runtime Config input source 结构化改写为 sealed file path。

结果如下：

| Arm | Run ID | Process | stdout | Final result |
| --- | --- | ---: | ---: | --- |
| P-detailed | `run-01a02d53-70b5-76b1-9aa9-a304ee90ab31` | exit 0，约 4.20 秒 | 55,493 bytes | exact pass |
| P-terse | `run-01a02d54-9e8e-7231-bd02-54c21d72e3ee` | exit 0，约 5.17 秒 | 64,692 bytes | exact pass |
| T-extra | `run-01a02d55-e903-7881-9952-84bc1daaf9ab` | exit 0，约 4.41 秒 | 49,867 bytes | exact pass |

三臂都只执行 `read task → read Skill → write result`，均未发生 tool error；T-extra 没有使用额外提供的 `bash`。三个 Final Image 中的 `/workspace/result.txt` 都是 exact bytes `RL::AMBER::52\n`，size 14，digest `sha256:ba9463bfbbf0548ceb8ba23490853cc706d6e2585d248e74e0fc1f39fb59e901`。这只能说明当前冻结任务的一次样本没有显示 Prompt 精细度或额外 `bash` 对 verifier 的影响，不能推广到其他任务，也不能支持统计结论。

三条 Run 从 `accepted_at` 到 `terminal_at` 分别约 54.08、55.63 和 54.09 秒。其中 acceptance 到目标进程 start 约 25.83–26.62 秒，目标进程 end 到 terminal 约 23.82–23.85 秒；Agent 进程本身只有约 4–5 秒。Run Record 有意不公开内部 lifecycle stage，因此这些事实不能继续区分 Image materialization、network setup、runtime setup、filesystem capture 和 publication 各自占比。它仍然构成 warmed-state 的真实性能摩擦，后续应通过内部 profiling 定位，而不是增加公共阶段或 CLI 语法来掩盖。

三个 `run verify` 均为 valid。复测后的 `state verify` 为 valid：6 条 terminal Run、0 accepted Run、0 orphan blob、0 staging entry、0 recovery entry。非敏感配置、stdout 和 verifier 文件保留在 `/private/tmp/runlab-pi-warm-e2e-20260823.IlB0ux`；临时认证文件在验证完成后删除，未进入报告、Git 或 OCI artifact。

## 保留的失败与修复

### 1. PATH 指向旧产品

首次实验时，普通 `runlab` 解析为 uv 安装的 Python 0.1.0.dev0，帮助仍声明 `Base + Overlay + Task -> Run`；当前仓库 binary 是 Rust 0.2.0-dev.0 Run Protocol。若调用方不主动固定 `target/release/runlab`，实验会进入错误产品面。

在 warmed-state 复测前已移除 uv 管理的旧 `runlab` tool，并用 `cargo install --path /Users/bytedance/workspace/temp/runlab-protocol --root /Users/bytedance/.local --locked --force` 把当前 Rust binary 安装为 `/Users/bytedance/.local/bin/runlab`。独立 zsh 中 `command -v runlab` 只返回该路径，`runlab --version` 为 `0.2.0-dev.0`，安装文件 SHA-256 为 `75d8b312458f0d8f2199743115515f5c83aa9cae6c626f2ced3423194a3d0fc6`，顶层 help 不再出现 Base/Overlay/Task。后续复测只调用普通 `runlab`，因此本机 PATH 冲突已经收敛；这不等于项目已经具有正式 release/install 流程。

### 2. Lima instance version 误判

首次 `vm create` 创建实例后失败：

```text
runlab: managed VM must use Lima 2.2.0, found 2.2.0
```

原因是 `limactl list --json` 返回 `"limaVersion":"2.2.0"`，实现却要求 `v2.2.0`。已修改 `src/managed_vm/host.rs` 按 Lima 2.2.0 的真实 JSON shape 精确接受 `2.2.0`，并新增接受/拒绝回归测试。修复后对首次已经创建的停止态实例执行 `vm status` 和 `vm start` 成功。

### 3. 缺少自动 guest artifact

仓库已有的旧 Linux binary 版本字符串相同，但缺少当前 `__internal-vm-handshake`，`vm install` 正确拒绝。为继续实验，调用方必须用 Linux/arm64 Rust container 从当前源码重新构建 guest binary。当前 macOS 开发版不能自动解析或下载与 host protocol 匹配的 Linux RunLab artifact。

### 4. Pi package identity 不可从 binary version 推断

本机 `pi --version` 是 0.84.2，但尝试安装 `@mariozechner/pi-coding-agent@0.84.2` 得到 npm `ETARGET`；本机实际 package 是 `@earendil-works/pi-coding-agent@0.84.2`。Agent Image authoring 需要记录 package name、version 和 registry integrity，不能只记录 CLI version。

### 5. transient systemd unit cleanup race

首次成功 guest `image import` 后，`vm exec` 仍退出 1：

```text
systemctl reset-failed failed ... Unit ... service not loaded
```

`guest_remove` 先 `systemctl stop` transient unit；stop 已使 unit unload，随后 `reset-failed` 必然可能看到 not-found。已从 `src/managed_vm/guest.rs` 删除多余的 `reset-failed`，保留 stop 成功作为 unit cleanup gate，再删除 operation directories。重新构建/安装 guest binary后，多次 `vm exec` 均正常清理。第一次 import 的业务结果已成功持久化，遗留 transport operation 通过内部 abandon 路径显式清理，没有重做或重复报告 import arm。

### 6. host auth readiness 不能替代 sealed credential readiness

首次 Arm A 使用本机已有 `~/.pi/agent/auth.json`，但 host `pi auth check` 的 ready 实际来自环境变量。Run `run-01a02d26-d402-72a2-aa11-3523fff6e4d7` 在 0.8 秒内 exit 1，stderr 为 `No API key found for deepseek.`。它不进入 Skill 比较。

随后生成只含 DeepSeek api-key entry 的私有临时 `auth.json`，在显式移除 host `DEEPSEEK_API_KEY` 后用 `PI_CODING_AGENT_DIR` 独立检查得到 ready，再从相同配置重跑 A 和 B。没有只修 B，也没有把失败 preflight 计入有效 Arm A。

### 7. 缺失 output slot 会遮蔽 guest 业务错误

对 Arm A Final Image 请求不存在的 `/workspace/result.txt` 时，guest command exit 1 且没有创建 declared output。host 最终报告的是 output slot `sha256sum: ... No such file or directory`，并保留 operation，而不是直接呈现 `image file get` 的原始业务错误。`vm operation get` 可见 terminal exit 1，`discard` 能安全清理，但 attach 同样会被缺失 output 阻塞。调用方可以判断 verifier 失败，诊断体验仍需要改进：terminal guest failure 的 stderr 应优先保留，缺失 output 应作为独立 publication fact，而不是遮蔽原始错误。

## 当前支持度判断

支持良好的部分：

- 单次执行输入由标准 OCI Image 与 Runtime Config 明确表达，两个 arm 的唯一变量可在 acceptance 前检查和 hash。
- Image import、egress、provider credential 的 read-only file seal、exact streams、capture limit、Final Image 和 cleanup 能组成真实 Agent Run。
- Agent 非零退出、capture limit、partial stream 和 Final filesystem 都作为事实保存，没有把失败压成一个布尔状态。
- `run verify`、`state verify` 和 Final Image file extraction 足以建立可审计证据。
- Secret projection 未进入 Final Image，state 最终没有 recovery/staging residue。

明显不方便或仍缺失的部分：

- 本机 legacy PATH 已经收敛，但项目仍缺少可验证的正式 release/install 流程；macOS 首次 setup 仍需手工准备 Lima、同版本 Linux binary 与 runc。它是 onboarding/delivery 问题，不应计入 warmed-state 实验循环。
- Agent Image 的 package、版本、registry integrity 和基础 Image provenance 仍必须由 Image authoring 方明确保存。RunLab 接受标准 OCI Image，不应为 Pi 增加专用 authoring DSL。
- warmed-state 三条短 Agent Run 各有约 50 秒花在目标进程以外。当前证据只定位到 acceptance 前后的 Run 阶段，尚不足以归因到具体内部机制。
- `vm exec --output` 在 guest failure 且 output 缺失时的错误优先级不利于诊断。

前一轮曾把“需要 opaque argv convenience、provider-neutral Secret Binding、命名 file slot、Experiment runner 和 `run diff` scope”列为潜在改进。warmed-state 复测没有显示标准 OCI JSON、数字 slot 或原子命令导致 Agent 操作错误；Experiment/judgment 外置和完整事实 diff 又是明确设计边界。因此这些项目当前均为证据不足，不应据此增加公开语法或 RunLab 模型。若后续真实工作流反复出现同类失败，再重新评估。

## 原始证据位置

- Guest state namespace：`pi-skill-e2e`。
- Host 非敏感构建与 stream evidence：`/private/tmp/runlab-pi-skill-e2e-20260823`。
- Warmed-state 非敏感配置、stream 与 verifier evidence：`/private/tmp/runlab-pi-warm-e2e-20260823.IlB0ux`。
- RunLab Run identities 和 OCI Manifest digests 见上文，可通过当前 VM namespace 的 `run get`、`run stdout get`、`run diff`、`run verify`、`image file get` 和 `state verify` 复查。
- 临时 credential file 不属于实验 evidence，实验完成后已经从 host 删除；未把它纳入报告、Git 或 OCI artifact。
