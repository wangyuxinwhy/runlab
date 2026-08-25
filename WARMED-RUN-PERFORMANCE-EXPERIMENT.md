# Warmed Run 性能基线实验

本文件记录 2026-08-23 在 macOS arm64 managed VM reference path 上执行的 warmed-state Run 性能基线。它是临时工程证据，不是 Run Protocol、公开 benchmark 或产品 SLO。

## 冻结实验合同

本合同在任何 baseline Run 结果产生前冻结。实验期间不修改 RunLab 实现、Initial Image、Runtime Config、Run Controls、重复次数或统计口径。若 evaluation path 发生变化，所有受影响 case 必须重跑；失败和异常值不能从结果中删除。

本实验只测已经准备好的执行循环，不计入 Lima、VM、guest RunLab、runc、Image pull、Runtime Config authoring 或 credential 准备。四个 case 串行执行，每个重复三次：

| Case | Initial Image | Process | Network | 目的 |
| --- | --- | --- | --- | --- |
| B0 | BusyBox arm64 | `true` | `none` | 最小 Image、无 filesystem 变化的固定 Run 成本 |
| B1 | Pi Agent Image | `/bin/true` | `none` | 大 Image materialization 对比 |
| B2 | Pi Agent Image | 写入 14-byte `/workspace/result.txt` | `none` | 小 filesystem 变化与 Final Image capture 对比 |
| B3 | Pi Agent Image | 既有 Pi Skill smoke task | `egress` | 加入模型请求与三次工具调用 |

所有 case 使用 timeout 180 秒、stdout/stderr limit 各 1,048,576 bytes、empty stdin 和 native Linux backend。B3 credential 只通过已有 Runtime Config file slot 投影，不能进入 accepted Runtime Config bytes、报告或 Final Image。

冻结输入：

- Guest state namespace：`pi-skill-e2e`；
- BusyBox remote index：`sha256:9db7b59979c38555a39def84a31fb98b5296952f9e3afd4f6f11f05b07adfab0`；
- B0 Initial Manifest：`sha256:f10e809bcf667d8e9f01d2baf82869049a495cd448cdfe1f4dee94078b960ae9`；
- B1–B3 Initial Manifest：`sha256:7042ff155aba55ca49113a190ba0d153ca7918777376b3839cc2a5aeb316c345`；
- B0 source Runtime Config SHA-256：`9c6c0891f3314f21bffcd2fc375c3124f82fbf659295b8ccb3fa810e8266a6bc`；
- B1 source Runtime Config SHA-256：`eaf1470ddef2e1d0f4734b4b4ed66e07ea8022148aeb434e9751953f5cc07e52`；
- B2 source Runtime Config SHA-256：`aeed67f51df61a34bf6e81e10790a6544a0536be510e3c69a8242d8ca5fe4100`；
- B3 source Runtime Config SHA-256：`088655ea68a117ff22c6ab4b14cb504a09ee7a8f599e706f4608cb5f33229d05`；
- B3 model：`deepseek/deepseek-v4-flash`，thinking `low`；
- B3 Skill、task、Prompt 和 tools 与 `PI-AGENT-EXPERIMENT.md` 的 P-detailed arm 相同。

原始非敏感输入和后续输出保存在 `/private/tmp/runlab-warm-baseline-20260823.2axCbS`。临时 credential 在实验结束后删除。

## 测量与解释

每条 Run 保存：host `vm exec` wall time、`accepted_at`、process start/end、`terminal_at`、process outcome、stream facts、Initial/Final Image identity、operation errors 和 cleanup facts。分别计算 acceptance-to-start、process duration、end-to-terminal 和 accepted-to-terminal；不把公共 Run Record 没有证明的内部阶段归因给 materialization、network、runtime 或 capture。

B0–B3 只用于定位性能层，不评价 Agent 能力。三个重复提供当前环境中的离散程度，不构成跨机器 benchmark。正式结论必须同时报告所有 12 条 Run。

## 准备阶段发现

`image pull docker.io/library/busybox:1.37.0` 被客户端直接请求为 `https://docker.io/v2/...` 并 connect timeout；改用明确 Distribution host `registry-1.docker.io/library/busybox:1.37.0` 后成功。这次失败发生在 Run acceptance 前，不属于 baseline Run 数据。是否需要 Docker Hub alias 处理要结合更多 registry 工作流判断，不能据此增加新的公开 Image 语法。

## 结果

12 条 Run 全部被接受并 terminalize，process exit 0，Final Image 可用，stdout/stderr facts 可用，operation error 为空，cleanup 报告 resources absent。时间如下，单位均为秒：

| Case | Run ID | Host wall | Accept→start | Process | End→terminal | Accept→terminal |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| B0 | `run-01a02d93-9a9b-7563-a0d5-67770d359561` | 10.95 | 0.623 | 0.170 | 0.898 | 1.691 |
| B0 | `run-01a02d93-dd17-7a83-a869-01e786fc35ea` | 10.69 | 0.624 | 0.159 | 0.874 | 1.657 |
| B0 | `run-01a02d94-1f31-7412-a0e4-cb816dbdbc4c` | 10.99 | 0.616 | 0.165 | 0.870 | 1.651 |
| B1 | `run-01a02d94-7e05-7d50-8a7a-784f54af3df7` | 63.96 | 25.335 | 0.168 | 24.037 | 49.540 |
| B1 | `run-01a02d95-96f9-75d1-ba6e-108530830663` | 61.22 | 24.217 | 0.160 | 23.794 | 48.171 |
| B1 | `run-01a02d96-a54b-7f73-b276-c492e26bc112` | 61.24 | 24.448 | 0.168 | 23.962 | 48.577 |
| B2 | `run-01a02d97-b2a4-7e60-9d07-cc933e6427a1` | 61.78 | 24.361 | 0.160 | 23.636 | 48.157 |
| B2 | `run-01a02d98-c443-78b0-87ad-3d6fd3dc6e87` | 61.31 | 25.019 | 0.155 | 23.795 | 48.969 |
| B2 | `run-01a02d99-d685-7dd2-a5c0-e02ecd445d6c` | 61.66 | 24.187 | 0.160 | 23.497 | 47.845 |
| B3 | `run-01a02d9a-f503-7c01-836a-c06e92fe6dd0` | 68.18 | 25.851 | 4.008 | 23.796 | 53.655 |
| B3 | `run-01a02d9c-27fe-7f13-a997-7f4891fc47f2` | 70.01 | 25.958 | 4.498 | 23.425 | 53.881 |
| B3 | `run-01a02d9d-5c09-76e0-b2b0-59bce851df97` | 69.05 | 25.755 | 4.401 | 23.689 | 53.845 |

各 case 的 host wall median 分别为 B0 10.95、B1 61.24、B2 61.66、B3 69.05 秒。B0 到 B1 只改变 Initial Image 和 argv 所在 Image，目标进程仍为约 0.16 秒，但 accept-to-start 从约 0.62 秒增至约 24–25 秒，end-to-terminal 从约 0.87 秒增至约 23–24 秒。B2 实际写入一个 14-byte 文件，却与 B1 基本相同。B3 在同一大 Image 基线上增加约 4–4.5 秒 Agent process 和少量 egress/transport 成本。

因此证据支持：当前短 Agent Run 的主要 warmed-state 成本与大 Initial Image 的逐 Run 执行前准备和执行后 Final Image 路径相关，不是 Prompt、模型推理或 14-byte changeset 大小造成。公共 Run facts 仍不足以区分 materialization、rootfs verification、Overlay setup、filesystem walk、changeset encoding 和 content publication各自占比；下一步应做内部 profiling，不能把其中某一项写成已证实根因。

B2 和 B3 六个 Final Image 的 `/workspace/result.txt` 都是 exact bytes `RL::AMBER::52\n`，size 14，digest `sha256:ba9463bfbbf0548ceb8ba23490853cc706d6e2585d248e74e0fc1f39fb59e901`。

## Accepted Runtime Config identity 漂移

B0、B1、B2 每组三次重复各自保存相同 accepted Runtime Config digest：

- B0：`sha256:b632156ea31c6b5c0a6acf1d0403030f68c1c287649b32091481f117fee142a6`；
- B1：`sha256:7ec43f87a72970690a4dacb1d463a631e4f2a3ef1c64410ea886862f48e9a0b4`；
- B2：`sha256:d0b05c5daf6a8d5e18178f5eb41a0c0080c6b6196006ae1c166b9872dfd017b9`。

B3 三次使用相同 source Runtime Config bytes 和相同 credential bytes，却分别保存：

- `sha256:fe45d5fe06e3cebb8be37cda4f171fa27277a204fe7b6b03b729e740c1da247e`；
- `sha256:c75cb45b840239efac13e060575f1b069f0616bae008b2558e65c59c1ded34a9`；
- `sha256:851e4e2addd2fcbab87ef4fe651766437be33e1d01ffd6badb1e3e39a408e325`。

源码核验显示 VM sealing 把 mount source 改写为 `sealed_source_path(operation_id, index)`，operation-specific guest path 因而进入实际 accepted Runtime Config bytes。这个实现忠实保存了实际交给 native engine 的配置，也避免 host path 进入 guest；但它同时让重复实验的 config identity 被 transport realization 扰动。正确边界需要结合 source config provenance、sealed input identity、accepted execution config 和 backend realization 统一设计，不能通过忽略 digest、固定共享 secret path 或把 credential 写入 Run Record 来规避。

## 完整性状态

实验末态 `state verify` 为 valid：18 条 terminal Run、0 accepted Run、0 staging entry、0 recovery entry。B0 pull 下载的 9,535-byte source index 没有被只指向 selected arm64 Manifest 的 Catalog/Run 引用，因此报告 1 个 orphan OCI blob；它是显式 GC 候选，不是 Run 或 Store 损坏，本实验没有执行 GC。

临时 credential 在所有 Run 和 verifier 完成后删除，不能从实验目录恢复；非敏感 config 和 verifier 文件保留在冻结路径。
