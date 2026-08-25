# Pi Agent 真实仓库任务 Screening 实验

本文件记录 2026-08-23 在 RunLab macOS managed VM reference path 上执行的三任务、五 arm Agent screening。它是实验合同和实现证据，不是 Run Protocol、Skill 效果结论或产品 benchmark。

## 冻结边界

本合同在首条 Agent Run 前冻结。开始后不修改 task、production fixture、public tests、hidden verifier、Skill、Prompt、tools、model、Runtime Config、Run Controls、执行顺序或成功定义。evaluation path 如有任何变化，所有受影响 arm 都必须重跑。每个 arm 只运行一次，所有失败和异常值都保留，因此结果只用于 E2E screening，不支持统计推广。

RunLab 只执行单次 Run 并保存事实。Verifier 不是 RunLab 概念，而是实验调用方的外部下游程序；本实验只是选择把该程序也作为一条普通 Run 执行。三任务、arm 顺序、Final Image 传递和最终判断都由 RunLab 之外编排。

## 冻结 OCI Image

最终 Initial Image 为 `agent-screening:1`，Linux/arm64 Manifest `sha256:218e095a113e3d117c4f34c572e4d620c841e88414391fbf52cc32b3a95857ac`。OCI archive 为 `/private/tmp/runlab-agent-screening-20260823-v3.oci.tar`，SHA-256 `1154ffa357f005a1915367adc0d2dadb1b6b744c962ca1224b67c5a4b5205181`。

Image 固定包含：

- Node 24.3.0；
- `@earendil-works/pi-coding-agent` 0.84.2；
- `/experiments/query`、`/experiments/cache`、`/experiments/merge` 三个 buggy repository；
- `/opt/runlab-skills/repo-repair-workflow/SKILL.md`；
- 0-byte credential 和 verifier mount destinations。

Image 不包含 hidden verifier。fixture source 保存在 `experiment-fixtures/agent-screening`。

在最终 Image 冻结前保留了两轮 mount authoring 失败：

- v1 缺少 `/verifier`，三条预检 Run 在 `read_only_file_mount_destination` phase terminalize 为 `not_started`；
- v2 只有目录、缺少目标 regular file `/verifier/verify.mjs`，三条预检 Run 同样 terminalize 为 `not_started`。

v3 预置 0-byte target 后，三个 hidden verifier 均真正启动并在原始 buggy Image 上 exit 1：query `run-01a02dab-3add-7b81-be34-c6df73a56b8f`、cache `run-01a02dab-3b43-70d1-a0df-bbab0093063f`、merge `run-01a02dab-3b43-7a62-8307-ffc485008a02`。这些预检不属于 screening arm。

## Task、Skill 与 Verifier identity

| Input | SHA-256 |
| --- | --- |
| query task | `874cda85993fba328cfc66be8d2e803031c0432c77771fd8ba0d736983d5b828` |
| cache task | `669b9e8d1ff04c8de439050dd76445dcdbc4b3a918ac1fb0c8f34fc334193395` |
| merge task | `a0915f6bcc29f5e644f9875120519f46b8d25e60394d1e8bfc604a516d871a94` |
| repository repair Skill | `d0f77192972c4f7a5348021693723360e5692a1fd246702cb015faf24ad8d2e2` |
| query hidden verifier | `56de032f1763f5343ce88a34e06b7a9626a5d288f63336c860cc2b52f63c8b97` |
| cache hidden verifier | `6f866d3d163260c7d58711eac475ae55f8dd06e40760a155e2a418c79b093959` |
| merge hidden verifier | `c36d865d9b92a163fed5e53397d4543432239c6ad4232aa8ab5484c3984e532f` |

Skill 只描述通用 repository repair workflow：读取任务和 build metadata、先复现、定位 invariant、只修改 production code、运行 targeted/full tests。它不包含任务名、实现细节、gold answer 或 verifier 信息，不安装到用户全局环境。

## 五个 Arm

所有 arm 使用 `deepseek/deepseek-v4-flash`、thinking `low`、JSON output、no session、no extensions、no prompt templates、no context files。Agent timeout 300 秒，stdout limit 4 MiB，stderr limit 1 MiB，network `egress`。

| Arm | Skill | Prompt | Tools |
| --- | --- | --- | --- |
| R | repo repair Skill | 标准 | `read,write,bash` |
| S0 | `--no-skills` | 标准 | `read,write,bash` |
| P0 | repo repair Skill | `Complete TASK.md.` | `read,write,bash` |
| P2 | repo repair Skill | 程序化详细 Prompt | `read,write,bash` |
| T1 | repo repair Skill | 标准 | `read,write,bash,edit,grep,find,ls` |

标准 Prompt 要求读取 TASK、修复 repository、让完整 tests 通过并真实编辑文件。P2 进一步显式要求先跑测试、检查 metadata/production/tests、定位 root cause、做最小 production fix、跑完整测试且不修改 tests。

15 个 source Agent Runtime Config 已通过 `runtime-config check`，保存在 `/private/tmp/runlab-agent-screening-20260823.lBUOgV`。SHA-256：

| Task | R | S0 | P0 | P2 | T1 |
| --- | --- | --- | --- | --- | --- |
| query | `d42760aa…` | `28e30e97…` | `391493ff…` | `a820ed04…` | `a36afbe1…` |
| cache | `d66d458e…` | `a131cd75…` | `3d8e5401…` | `361b35fd…` | `229931d5…` |
| merge | `6304d396…` | `add44a72…` | `488a31a7…` | `d4a83e5e…` | `2a26594d…` |

表中省略号只用于阅读，完整 digest 由同目录源文件和实验开始前的 `shasum -a 256` 输出保存。

首次自动执行尝试发现 `runtime-config create` 生成的默认 OCI config 含 `network` namespace，而 native `Run Control network=egress` 要求 config 只含 `pid, ipc, uts, mount, cgroup`。query/R、cache/S0、merge/P0、query/S0 的自动尝试以及一次 query/R 手工复现均在 acceptance 前失败，没有 Run identity。所有 15 份 Agent config 随后统一删除标准 OCI `network` namespace、重新 check 和 hash；上表是修正后的最终 identity。由于没有 Agent Run 被接受，这些失败不计入 screening，正式顺序从 query/R 重新开始。这个事实同时表明 `runtime-config create` 的默认 output 与 egress Run Control 不能直接组合，需要在 authoring guidance 或接口契约中明确。

## 外部验收程序

实验调用方把每条 Agent Run 的 Final Image 传给对应的外部验收程序。为了同样获得可审计执行事实，本实验把该程序也作为一条普通 Run 启动：Runtime Config 通过 read-only file slot 把冻结程序投影到 `/verifier/verify.mjs`，执行 `node /verifier/verify.mjs`，timeout 60 秒、stdout/stderr limit 各 1 MiB、network `none`。

“该普通 Run 的 process exit 0 就算实验成功”是外部调用方的 judgment，不是 RunLab 语义。Agent 自报、public tests 输出或 Final Image 存在都不单独构成本实验的成功。该普通 Run 的 file slot 会触发已知 operation-specific accepted Runtime Config digest 漂移；source config 和验收程序 bytes identity 仍保持冻结并单独记录。

## 冻结执行顺序

为避免五个 arm 总是处于相同时间位置，按轮次交错任务和 arm：

1. query/R、cache/S0、merge/P0；
2. query/S0、cache/P0、merge/P2；
3. query/P0、cache/P2、merge/T1；
4. query/P2、cache/T1、merge/R；
5. query/T1、cache/R、merge/S0。

每个 pair 先完成 Agent Run，再立即以其 Final Image执行 Verifier Run。不会因为前一 arm 成功或失败而跳过后续 arm。

## 结果

15 条 Agent Run 都被接受并 terminalize，process exit 0，Final Image 可用，stderr 0 bytes，operation error 为空。每个 Final Image 后续的 hidden Verifier Run 也都 process exit 0，stdout 34 bytes，stderr 0 bytes，operation error 为空。

| Task | Arm | Agent Run | Agent process | Verifier Run | 判定 |
| --- | --- | --- | ---: | --- | --- |
| query | R | `run-01a02db0-dc2d-7330-9886-eaee4bcdf4aa` | 12.481 s | `run-01a02db2-361b-7aa0-b6c5-7eac3588230d` | pass |
| query | S0 | `run-01a02db8-25a5-7410-b2e8-fc3d071c0d71` | 12.217 s | `run-01a02db9-4b77-75e3-be7e-40eeefd55d39` | pass |
| query | P0 | `run-01a02dbe-c236-7b20-ae22-7e822c18c1a8` | 11.752 s | `run-01a02dbf-e50b-7460-aad8-2f6d29d22657` | pass |
| query | P2 | `run-01a02dc5-63e8-7863-bfb9-74025eb46923` | 14.907 s | `run-01a02dc6-91dc-7460-800f-2c5433ad2ab3` | pass |
| query | T1 | `run-01a02dcb-e255-74e1-852f-b30a4e0009dd` | 11.982 s | `run-01a02dcd-09d6-7a03-a07c-394d33dd3429` | pass |
| cache | R | `run-01a02dce-032f-7f63-8a76-6696a7b5fd62` | 20.239 s | `run-01a02dcf-4634-7712-b3be-9e9040d8374b` | pass |
| cache | S0 | `run-01a02db3-9e91-7f32-9c5f-ac219ee21e8f` | 23.310 s | `run-01a02db4-f2db-7063-badb-318600cf9891` | pass |
| cache | P0 | `run-01a02dba-48c7-7b20-ac3e-c4d38ee57a58` | 14.705 s | `run-01a02dbb-76ff-7a52-a049-d51a0ec5abd7` | pass |
| cache | P2 | `run-01a02dc0-e0cc-7c21-a1ab-eb5621198885` | 15.153 s | `run-01a02dc2-1121-7cf1-aa42-0daacb05c451` | pass |
| cache | T1 | `run-01a02dc7-8d6f-7450-9db7-5af9325b5842` | 10.680 s | `run-01a02dc8-afa5-7091-8ec6-e5e3efce4e46` | pass |
| merge | R | `run-01a02dc9-ae38-7de3-b5d2-79e92882d5dc` | 16.896 s | `run-01a02dca-e847-78a0-bca0-462443938ea3` | pass |
| merge | S0 | `run-01a02dd0-42b4-71c2-8658-83aa35aec9e8` | 17.826 s | `run-01a02dd1-7b8d-7b92-bb94-97626bd9f635` | pass |
| merge | P0 | `run-01a02db5-eb38-72a1-b297-cdcab704c06b` | 15.721 s | `run-01a02db7-25ed-7b53-95af-4dae23b6c0a9` | pass |
| merge | P2 | `run-01a02dbc-72dc-74e0-b758-5aac75deabe1` | 23.518 s | `run-01a02dbd-c7ca-75b0-bc23-fc70710da1ff` | pass |
| merge | T1 | `run-01a02dc3-14f8-7382-8a48-eb393664b6c5` | 22.344 s | `run-01a02dc4-6a56-7ba0-b18b-eba92cef386e` | pass |

Pi JSON trace 从 RunLab 保存的 exact stdout 中取回后，得到以下行为事实。“编辑前测试”只计入在首次 `write`/`edit` 前真正运行过的测试命令。

| Arm | Verifier pass | 读取 Skill 正文 | 编辑前测试 | 每任务测试命令数 | 平均 Agent process | 平均 trace cost |
| --- | ---: | ---: | ---: | --- | ---: | ---: |
| R | 3/3 | 0/3 | 0/3 | 1, 1, 1 | 16.539 s | $0.0008452 |
| S0 | 3/3 | n/a | 0/3 | 1, 1, 1 | 17.784 s | $0.0009334 |
| P0 | 3/3 | 0/3 | 0/3 | 1, 1, 1 | 14.059 s | $0.0007561 |
| P2 | 3/3 | 0/3 | 3/3 | 2, 2, 2 | 17.859 s | $0.0009424 |
| T1 | 3/3 | 0/3 | 0/3 | 1, 1, 1 | 15.002 s | $0.0009717 |

P2 在三个任务上都按 Prompt 显式地先复现、后修改、再测试；其他 arm 都直接定位和修改，仅在修改后运行一次测试。T1 确实使用了新增的 `ls`，query/T1 还使用了 `edit`；但 cache/T1 和 merge/T1 仍用 `write`。所有 trace 都没有 tool error。

## Skill 处理效度

这一轮不能用于判断 Skill 正文的效果。Pi 0.84.2 的本地文档和实现表明：`--skill <path>` 仅在 system prompt 中暴露 Skill 的 name、description 和 location，模型需用 `read` 按需加载正文；`/skill:name` 才会强制展开正文。Pi 文档也明确说模型不一定会自动读取。

R、P0、P2、T1 的 12 条 trace 都没有读取 `/opt/runlab-skills/repo-repair-workflow/SKILL.md`。因此这些 arm 只证明“Skill 可发现”，不证明“Skill 已调用”；R 没有遵循 Skill 中的编辑前先测试要求与此一致。不能把 R 与 S0 的相同成功率解释成 Skill 无效，也不能把 R 的成功归因于 Skill。

## 当前能支持的结论

- RunLab 的原子 Run 能稳定执行真实 Pi Agent，保存完整 stdout/stderr/process/Final Image 事实，并把 Agent 的 Final Image 作为独立 Verifier Run 的 Initial Image；15 个 Agent→Verifier pair 全部跑通。
- 详细 Prompt 在这三个任务上可靠改变了过程行为，但没有提供更高正确率；所有 arm 已经 3/3，存在明显 ceiling effect。
- 更大 tools 集合被 Agent 真实使用，但没有一致降低 process time、token 或 cost。单次样本不支持排名或显著性结论。
- Agent 自报不是判定依据。这一轮的成功率来自隐藏 verifier 对 Final Image 的独立执行。

## RunLab 易用性发现

这些是实验中实际发生的 friction，不是为了预留扩展点而提出的语法设计。

1. 大 Agent Image 的逐 Run 准备和 Final Image 路径是最大的迭代成本。前述 warmed baseline 中，Pi Image 上的 `/bin/true` 中位 host wall 为 61.24 秒，其中 process 只约 0.16 秒；本轮 Agent process 也只有 10.68–23.52 秒。优先级应是 profiling 和优化现有 materialize/capture 实现，而不是添加跳过 Final Image 的公开快捷语法。
2. `runtime-config create` 的默认 OCI `network` namespace 与 `run start --network egress` 不能直接组合，直到 acceptance 时才得到精确错误。应先使 authoring output 与 Run Control contract 自洽，或让 check 在获得两者时验证组合；不需要新的 network DSL。
3. Read-only file mount destination 必须在 Image 中预先存在且是 regular file。两轮 accepted 后 `not_started` 才让 Image author 找到这个要求。该 invariant 应进入 Image authoring guidance；若 `run start` 在 acceptance 前已有足够 Image 事实，再考虑把它前移为组合验证。
4. File slot sealing 把 operation-specific guest path 写入 accepted Runtime Config bytes，使相同 source config 和相同 input bytes 在重复 Run 中得到不同 config identity。需要明确 source config provenance、sealed input identity、accepted execution config 和 backend realization 的边界，不应通过弱化 digest 或固定 secret path 掩盖。
5. RunLab 原子能力足够组合 Agent→Verifier，但 15 个 pair 的顺序、Final Image 传递、verdict join 和 trace 聚合全由外部脚本负责。这是第一个证明矩阵编排确实重复且易出错的真实工作流。下一步应先保留一个可审阅的 reference orchestrator 和明确的 machine-readable 输出合同，再由第二类实验判断是否值得进入一个小的公开 experiment surface。
6. macOS 实验只用 `vm exec --namespace ...` 作为 transport wrapper，guest 内仍是同一个 `runlab` 和 Run Protocol。Local 与 VM 不应被设计成两套 protocol context：Local 是 Linux 上的直接执行/开发路径，VM 是 macOS 的明示 transport adapter；文档应用这个关系解释 `--state` 与 `--namespace`，而不是增加泛化 context abstraction。

## 未完成的 post-hoc Skill 强制调用诊断

上述 trace 暴露 Skill treatment 没有实际生效后，另行声明一个诊断性跟进。它在看到首轮结果后设计，因此不与首轮合并成 pre-registered benchmark，也不用来给 Skill 效果做统计推广。它只检查两件事：强制调用是否让正文真正进入 Agent context，以及现有 RunLab 组合路径能否再次稳定完成 Agent→Verifier。

不修改三个 task、production fixture、public tests、hidden verifier、Skill 文本、Image、model、tools 或 Run Controls。两个 arm 在三个任务上都重新运行，不复用首轮 S0 结果：

- F：使用同一 Skill path，把用户输入改为以 `/skill:repo-repair-workflow` 开头，强制 Pi 把冻结 Skill 正文展开到 prompt；
- C：新的 `--no-skills` control Run，使用首轮标准 Prompt。

顺序冻结为 query/F、cache/C、merge/F、query/C、cache/F、merge/C。每个 pair 仍先 Agent，然后对 Final Image 运行原 hidden verifier。成功只由 verifier exit 0 定义；每个 arm 仅一次，不按结果重试。

第一次 query/F `run-01a02ddc-e829-7492-82a5-695758654546` 使用了不含 DeepSeek entry 的 host `auth.json`，process 在 0.81 秒后 exit 1，stderr 为 `No API key found for deepseek.`。这条 Run 只能作为 credential preparation 失败保留，该诊断合同因 treatment 未进入模型而作废。

生成 provider-specific 临时 credential 并验证结构后，曾从头启动新一轮，完成了前四个 pair：

| Task | Arm | Agent Run | Agent exit | Verifier Run | Verifier exit |
| --- | --- | --- | ---: | --- | ---: |
| query | F | `run-01a02de0-29bf-75a3-95ae-e4d687a701e6` | 0 | `run-01a02de1-510f-70e0-ae5d-ad253064651e` | 0 |
| cache | C | `run-01a02de2-7722-7452-a371-2c27332c06ae` | 0 | `run-01a02de3-a06a-77b1-8307-57f30ecaf5d6` | 0 |
| merge | F | `run-01a02de4-c227-74d1-9814-fa8d90de0074` | 0 | `run-01a02de5-f6f6-7ad0-9fa1-ccedb2381331` | 0 |
| query | C | `run-01a02de7-1c09-7e62-ae5a-ad9dd1826ba9` | 0 | `run-01a02de8-34a9-7010-b4e2-695be0e9a525` | 0 |

此时经人工复核发现工作目标已从“评估 RunLab 端到端实验工作流”偏移为“补做 Pi Skill benchmark”，因此立即中止。cache/F 和 merge/C 没有被 acceptance，临时 credential 随后删除。因为这一轮本身是 post-hoc、又没有完成冻结矩阵，上述四个 pass 不进入 Skill、Prompt 或 tools 的对比结论，也不作为继续补跑的理由。

中止后的最终 `state verify` 为 `valid=true`：66 条 terminal Run、0 accepted Run、0 staging entry、0 recovery entry。仍只有前述 BusyBox pull 留下的 1 个 9,535-byte orphan source index。
