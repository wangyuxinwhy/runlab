# RunLab Agent 实验易用性设计

本文是 2026-08-23 Pi Agent 端到端实验之后的本地实施设计，不是 Run Protocol 的第二份规范。稳定产品和协议边界仍由 [Agent Wiki](http://localhost:8787/app/pages/runlab-index--nw) 拥有；本文只把当前实验证据转换为待实施的优先级。

## 结论

当前 Run Protocol 已经能作为 Agent 实验的可复现执行与事实层。真实 Pi Agent 能在 Initial OCI Image 中执行，其 filesystem 结果能以 Final OCI Image 保存，并交给 RunLab 之外的下游程序继续处理。问题不在核心模型缺少 Prompt、Skill、Task 或 Experiment 语法，而在现有标准对象的 authoring 组合、acceptance preflight、warmed-run 性能和外部证据编排还不够顺手。

因此目标不是建立 RunLab 私有实验 DSL，而是：

1. 让 RunLab 自己生成的输入能与常用 Run Controls 直接组合；
2. 把已可以在 acceptance 前证明的错误前移，不生成无意义 Run；
3. 在不改变“每次 Run 都有 Final Image”的前提下降低大 Image 的重复成本；
4. 让普通外部 driver 能可审阅地组合 Run，暂不把 Experiment 和 judgment 收入 RunLab Core。

## 设计边界

```text
Upstream experiment program（RunLab 之外）
  Task / Prompt / Skill / tools / arm / order / repeats / judgment
                 |
                 | standard OCI objects + Run Controls
                 v
RunLab
  resolve + verify + preflight -> accepted -> execute -> terminal facts
                 |
                 | Final OCI Image descriptor
                 v
Downstream program（RunLab 之外）
  inspect Image, start another ordinary Run, or apply any external judgment
```

RunLab 仍只拥有单次 Run 的可验证输入、lifecycle 和事实。Task、Prompt、Skill 和 tools 都是 Agent process argv、Image content 或 file input，不是 Run Protocol 字段。Verifier 不在 RunLab 中：它只是实验调用方命名的外部下游任务或程序。调用方可以选择把它作为普通 process 再交给 RunLab 执行，也可以完全在 RunLab 之外运行。RunLab 不识别 verifier 身份，不产生 verdict，不拥有它们之间的组合关系。

这个边界与 Agent Wiki 已定设计一致：Run Protocol 不定义 Experiment、Matrix、重复策略、评分或排名；Run Controls 只补充 OCI Runtime Specification 不拥有的 supervisor 行为。

## 实验证据

本设计只使用已完成且未污染的事实：

- warmed baseline 的 12 条 Run 全部 terminal、exit 0；BusyBox `true` 的 host wall 中位数为 10.95 秒，Pi Image `/bin/true` 为 61.24 秒，写入 14 bytes 为 61.66 秒，Pi Agent 为 69.05 秒；
- 三个代码任务、五个 arm 的 15 个 Agent Run 和 15 个 hidden Verifier Run 全部 exit 0；
- 所有 Agent Run 都产生可用 Final Image，Verifier 只基于该 Image 运行，没有使用 Agent 自报作为成功依据；
- 正式 screening 完成后 `state verify` 为 `valid=true`，57 条 Run 全部 terminal，0 accepted、0 staging、0 recovery；唯一 orphan 是 BusyBox pull 保留的 9,535-byte source index；
- post-hoc Skill 强制调用诊断在中途停止，不进入本设计的效果判断。
- 中止后的最终 `state verify` 仍为 `valid=true`，包含该诊断已产生事实在内共 66 条 terminal Run，0 accepted、0 staging、0 recovery；orphan 仍只是同一 9,535-byte source index。

详细 Run ID、digest、时间和失败见 `WARMED-RUN-PERFORMANCE-EXPERIMENT.md`、`PI-AGENT-EXPERIMENT.md` 和 `AGENT-SCREENING-EXPERIMENT.md`。

## 目标设计

### 1. Runtime Config authoring 必须表达真实 network 选择

当前 `runtime-config create` 只能产生带 private OCI `network` namespace 的默认 config。它可以用于 `run start --network none`，却不能用于 `--network egress`：egress 需要 participant 继承 Run-owned network namespace，因此 config 必须省略 OCI `network` namespace。这不是两个可互换的默认值，而是一个必须在 authoring 时知道的语义选择。

设计为给现有命令增加与 Run Control 同词汇的明示选项：

```text
runlab runtime-config create --network none   ...
runlab runtime-config create --network egress ...
```

`none` 仍可作为默认以保持当前安全语义；egress 只改变生成的标准 OCI namespace 列表。该选项不进入 Runtime Config bytes 之外的第二套模型，也不引入 profile DSL。`run start` 仍在 acceptance 前以实际 config、Backend 和 Controls 做最终 capability preflight。

`runtime-config check` 保持 backend-neutral，不在这里混入 native Run Control 语义。

### 2. 已可证明的 file mount 错误必须前移到 acceptance 前

实验中 read-only file mount 的 destination 缺失或只有父目录时，Run 被接受后才在 `read_only_file_mount_destination` phase 以 `not_started` terminalize。此时 RunLab 已经拥有 resolved Initial Image、accepted Runtime Config 候选字节和 mount destination，因此“destination 在 Initial Image 中存在且是 regular file”应当是 acceptance preflight。

目标行为：

- 缺失 destination、destination 不是 regular file，或路径穿越不被支持的 symlink 时，`run start` 在分配 `run_id` 前拒绝；
- error 指向精确 `mounts[i].destination` 并说明 Initial Image 中观察到的类型；
- 不自动创建 destination，不将 file mount 改写成 directory mount，不修改 Image；
- 新增独立进程 CLI 测试，证明拒绝后 Run 数量不变。

这不是便利功能，而是让实现符合已定 acceptance 责任边界。Image authoring guidance 仍要给出预置 0-byte ordinary file 的普通 OCI 做法。

### 3. 优化 warmed Run，不跳过 Final Image

性能证据已将主要成本定位在大 Image 的 process 前后，但公共 Run facts 不足以区分 materialization、rootfs verification、Overlay setup、filesystem walk、changeset encoding 和 content publication。实施顺序必须先 profiling，再修正已证明的热点。

Profiling 是实现内部证据，不把 materialize/capture 变成新的公开 lifecycle state。后续候选优化只在 profiling 支持时进入实现：

- 以 Manifest digest 为 key 复用可丢弃的 verified/materialized lower rootfs cache；
- 在不弱化 exact-byte verification 的前提下避免对相同 Initial Image 重复做无意义工作；
- 只有 Overlay upperdir decoder 与当前 walking oracle 在完整 corpus 上等价后，才用 changes-only capture 替代昂贵路径。

不增加 `--no-final-image`、`fast run` 或空变更时直接返回 Initial Image。Run Protocol 要求每次成功 capture 产生一个 child Image，即使 filesystem 没变化也不改变这个语义。

### 4. 先用 reference driver 验证编排边界

15 个 Agent→Verifier pair 证明矩阵编排是真实重复工作，但只有一类工作流证据，尚不足以固化公开 Experiment 模型。下一步应保留一个普通、可审阅的 reference driver，只组合已有 CLI JSON：

1. 在看到结果前冻结 task/arm/order/repeat 和各 source file digest；
2. 对每个 arm 调用一次 `run start`，记录 Agent Run ID 和 Final Manifest；
3. 把该 Manifest 交给外部验收程序；若调用方选择使用 RunLab 执行它，记录这条普通 Run 的 ID；
4. 以 NDJSON 保存 `task_id, arm_id, source digests, agent_run_id, final_manifest, verifier_run_id, verdict`；
5. 不自动重试失败 arm，不覆盖已有记录，不在运行后修改 evaluator。

driver 不得定义新 Run 身份、包装 stream facts 或把 verdict 写回 Run Record。它只是一个可替换调用方。只有第二类真实实验也重复出现相同 orchestration failure，才重新评估是否需要很小的上层 `experiment` 产品面。

### 5. Source Config identity 和 accepted identity 保持分离

VM file slot sealing 需要把 host placeholder 改写成 operation-specific guest path。实际交给 native engine 的 bytes 因此在重复 Run 中拥有不同 digest。这不应通过忽略 accepted digest、使用共享固定 secret path 或不保存实际 config bytes 修复。

当前实验已经证明一个无需协议变更的做法：外部 driver 在 sealing 前保存 non-secret source Runtime Config digest，Run Record 保存 accepted execution config digest。前者用于 arm 输入对比，后者用于审计实际执行字节。暂不把泛化 provenance 或 secret identity 字段加入 Run Record。

### 6. Local 与 VM 是执行位置，不是两个 protocol context

RunLab 只有一个 product surface：

- Linux 上直接执行 `runlab --state ... run start ...`；
- macOS 上用 `runlab vm exec --namespace ... -- run start ...` 把同一原子命令交给 managed Linux guest。

`--state` 是 Linux data plane 中的 State Directory；`--namespace` 是 macOS VM transport 用来分隔 guest State 的 host-side selector。不添加 `context create/use`、默认 context、环境切换或 Local/VM 多态抽象。易用性修正只需要在顶层 help 和 macOS how-to 中清楚说明这个 transport 关系。

### 7. 首次准备和重复实验分开交付

首次准备的真实问题是交付闭环：当前 macOS 开发版缺少自动解析与 host protocol 匹配的 Linux RunLab artifact，Pi Image author 还需要自行锁定 package name、version、integrity 和 base Image provenance。这些应继续进入 macOS delivery 和 Image authoring 工作，不能用它们否定 warmed loop，也不能为 Pi 增加专用 Image DSL。

日常重复实验只应要求已运行 VM、已安装 engine、已导入 Image、冻结 Runtime Config/source inputs 和显式 Run Controls。它的主要产品指标是每个新 arm 的准备步数、失败可诊断性和 warmed wall time，而不是安装文档的长度。

## 明确不做

- 不在 Run Protocol 中增加 Prompt、Skill、Task、tools、Experiment、Matrix、score 或 verdict 字段；
- 不增加 Pi-specific Image builder、credential provider 或 verifier 命令；verifier 本身不属于 RunLab；
- 不为了速度提供跳过 Final Image 的公开开关；
- 不把 numeric file slot 替换成命名 binding DSL；本轮没有证据表明 slot numbering 是主要错误来源；
- 不引入 Local/VM context abstraction；
- 不从单一 native Backend 提前抽象 generic Backend trait；
- 不将 post-hoc、未完成或 Skill 未真正调用的结果写成产品效果证据。

## 实施顺序

### P0：修正已证明的契约断点

1. 为 `runtime-config create` 增加 `--network none|egress`，保证两种 output 分别与同值 `run start --network` 组合成功，交叉组合在 acceptance 前失败。
2. 将 read-only file mount destination 的 Image existence/type 验证前移到 acceptance 前，覆盖 missing、directory、symlink 和 regular file。
3. 修正 `vm exec --output` 的错误优先级：guest 命令失败和 output publication 缺失必须同时可见，不让后者遮蔽前者。

Exit gate：三个失败都有独立进程 CLI contract test，验证 stdout、stderr、exit status 和 Run 数量；不把当前错误行为钉成新合同。

### P1：用 profiling 驱动 warmed-loop 优化

1. 在实现内部测量 materialize、runtime setup、capture/encode 和 publish，不改公共 lifecycle schema；
2. 使用 B0–B3 同等合同重跑所有 case，保留修改前后全部重复；
3. 仅对 profiling 证明的热点实施 cache 或 capture 优化，重跑 OCI integrity、Run/state verify 和真实 Docker/native 门禁。

Exit gate：能用内部数据说明时间降在哪个已证明阶段，且 Final Image、stream、process、cleanup 和 recovery 语义不变。

#### 2026-08-24 实施结果

P0 三项已经按上述边界实现：`runtime-config create --network none|egress` 生成可与同值 Run Control 组合的标准 OCI config；native file mount destination 在 acceptance 前按原 `mounts[i]` 索引批量校验；VM attach 在 output publication 失败时仍返回 guest stdout/stderr，并保留 operation 供重试或 discard。对应的独立进程和真实 VM 负例都证明失败没有被伪装成已接受或已完成的 Run。

P1 使用同一 B0–B3 输入、顺序和 3 次重复做修改前后对照。唯一例外是 B0 修改前第 1 次：Run 本身成功且原始 profile 完整，但 host 脚本误用了 zsh 只读变量 `status`，因此缺少 wall metadata；没有补跑或用别的结果替代。原始证据保留在 `/private/tmp/runlab-p1-profile-before-20260824` 和 `/private/tmp/runlab-p1-profile-after-20260824`。

| case | 修改前 host wall | 修改后 host wall | 中位数变化 |
| --- | --- | --- | --- |
| B0 small `/bin/true` | 11, 11 秒（n=2） | 12, 13, 12 秒 | 11 → 12 秒；VM transport 开销占主导，不支持改善结论 |
| B1 Pi Image `/bin/true` | 68, 68, 67 秒 | 44, 44, 39 秒 | 68 → 44 秒，降低 35.3% |
| B2 Pi Image exact write | 64, 60, 69 秒 | 40, 40, 40 秒 | 64 → 40 秒，降低 37.5% |
| B3 Pi Agent + egress | 72, 70, 73 秒 | 50, 53, 50 秒 | 72 → 50 秒，降低 30.6% |

内部 profile 将大 Image 的下降定位到三个实现变化，而不是公开快速路径：每个 Run 对同一 Initial Image 的完整 inspection 从 5 次降到 1 次；final tree 第一遍仍完整计算 inventory 和 content digest，但不再把之后会丢弃的 content 写入临时 store；Final Image 继续验证新 Layer 和新 exact-byte config/manifest，但复用 acceptance 前已完整验证的 parent `ImageView`，不再扫描不可变 parent Layers。B1/B2/B3 的 `run.execute` 中位数分别从 51.78/49.16/52.43 秒降到 29.56/28.22/33.79 秒；`native.final_tree_capture` 从 18.83/17.05/16.82 秒降到 8.31/8.05/8.11 秒；`native.final_image_capture` 从约 6.4 秒降到约 0.09 秒。

修改后 12/12 个 Run 都是 process exit 0、Final Image available、0 operation errors、cleanup resources absent。B2/B3 六个 Final Image 中的 `/workspace/result.txt` 都是同一精确 14 bytes，SHA-256 `ba9463bfbbf0548ceb8ba23490853cc706d6e2585d248e74e0fc1f39fb59e901`。全 namespace `state verify` 返回 `valid: true`，覆盖 92 个 Run、181 个 Image roots 和 294 个 reachable OCI blobs；它同时诚实报告 1 个 9,535-byte orphan blob，本轮没有用破坏性 GC 隐藏该事实。真实 Docker interruption/capture 合同和真实 native Run 都通过。

修改前 profiling binary 是 `sha256:2036f9cbc2d0f29d6d44b37cc3bd37624f062522ebd100e950d479a056ede568`；修改后是 `sha256:b233fab58128433165339f10daa24ff6bb8af141b4eab552964ecfd50d614e7a`。修改后二进制曾被误用 Rust 1.97.0 构建，但随后用项目固定的 Rust 1.97.1、相同 `--locked` 命令重建，所得 size 和 SHA-256 与已执行 artifact 逐字节相同，因此实际 Run artifact 没有工具链差异。若 digest 不同，这一组结果应全部作废并重跑。

剩余最大热点是大 Image 的逐 Run materialization，B1/B2/B3 中位数仍为 19.31/18.53/20.05 秒。Manifest-keyed materialized lower cache 可能继续降低 warmed-loop 成本，但它需要先明确私有 cache 的原子发布、完整性重验、ownership profile、并发、容量和 GC 边界；本轮不为追逐数字引入未设计的持久状态，也不增加公开语法。

### P2：交付 reference workflow，不急于公开 Experiment surface

1. 用第二类真实 Agent 工作流验证上述 NDJSON driver contract；
2. 补充 macOS how-to 中 Local engine 与 VM transport 的责任图、egress config authoring 和 file destination invariant；
3. 结合 clean-host delivery 处理 guest artifact 和安装 identity，不在 experiment driver 中隐式修复环境。

Exit gate：一个新 Agent 工作流可以从已准备状态只通过标准 OCI 文件、Run Controls 和既有 JSON 结果完成 Agent→Verifier；任何失败不需查数据库或猜测隐藏状态。

## 判断标准

后续每一项易用性改动都要回答三个问题：

1. 它修正了哪一个已经发生的工作流失败或重复成本？
2. 它是在完善现有 OCI/Run/VM 语义，还是引入了一个只为人类简写的新概念？
3. 如果移除这项改动，Agent 是仅多写几个字段，还是会丢失可发现性、正确性、可诊断性或可复现性？

只有第三个问题的答案是后者，且实验证据能定位具体失败时，才增加新的公开能力。
