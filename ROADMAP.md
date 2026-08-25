# RunLab 剩余工程路线

本文只记录尚未闭合的交付门禁。稳定目标与架构由 [Agent Wiki](http://localhost:8787/app/pages/runlab-index--nw) 拥有；当前已实现范围见 [IMPLEMENTATION.md](IMPLEMENTATION.md)。完成历史不在这里重复保存，Git 与版本化验证证据负责追溯。

## 1. 固定可复查基线

- 在不覆盖用户侧工作区改动的前提下建立稳定、可复查的代码基线，再重跑全部普通门禁；
- 在 Linux reference environment 重跑 native/rootless tests，并保存 runtime、kernel、filesystem 和权限事实；
- 重跑真实 Docker compatibility E2E，失败与首次尝试一并记录；
- 让 verification matrix 只引用同一代码基线产生的证据。

Exit gate：每项结果都能绑定 commit、环境和完整命令，未运行项不会被写成通过。

## 2. Native fidelity 与恢复矩阵

- 扩展真实 Linux distribution、kernel 和 filesystem 组合；
- 增加 capture/commit/terminal transaction 等 crash phase；
- 验证非 recovery-directory 的 host/runtime orphan discovery；
- 明确 atomic snapshot 方案，或继续把 two-pass capture 限制写入支持范围；
- 只有 upperdir decoder 与 walking oracle 在完整 corpus 上等价后，才考虑替换 production changeset source。

Exit gate：没有受测 crash point 丢失 accepted Run、发布含糊 Final Image 或留下无 owner 资源。

## 3. OCI Distribution 与资产来源

- 设计 credential boundary、bounded retry 与 registry compatibility matrix；
- 在不弱化 exact-byte verification 的前提下增加 push；
- 单独设计 referrer、signature 和 trusted provenance，不把 tag 当身份；
- 明确远端内容的保留承诺与本地 retention 关系。

Exit gate：读写两侧都能说明内容身份、凭证暴露面、失败原子性和可取回边界。

## 4. Docker compatibility 收口

- 为 stop 与 attached wait 增加独立 wall-clock deadline；
- 完成 default mounts、pid/mount namespace、安全和 daemon policy 的 fidelity 证明；
- 修正 asset 已发布但 temporary cleanup 失败时的 operation result；
- 继续让 Docker-only mechanics 留在显式 namespace，不进入核心数据面。

Exit gate：支持项有真实映射证据，不支持项在 acceptance 前拒绝，daemon 卡死不会让 Run 无界停留。

## 5. macOS 交付门禁

- 自动解析并验证匹配版本的 Linux RunLab artifact；
- 建立 RunLab-owned、长期可取回的 VM image provenance；
- 增加 host operation catalog 与安全 GC；
- 覆盖 transport-loss、disk-full、VM restart 和 engine upgrade failure matrix；
- 在 clean host 上验证相同 Run ID、JSON、bytes 与 cancellation semantics。

Exit gate：host 连接中断或 guest failure 不能产生 false success，也不会让 accepted Run 失去可恢复 identity。

## 6. 公开接口的后续能力

- 定义 JSON error schema 与兼容策略；
- 只有真实 workflow 证明需求后再设计 Secret provider/version 与 redaction；
- 扩展 Runtime field、resource、mount 或 service topology 前，为每项能力建立 acceptance 与 conformance gate；
- 不为尚未存在的消费者引入 generic Backend framework、SDK service、ORM 或 async runtime。

## 完成标准

任何阶段只有在代码、独立进程 CLI contract、真实环境证据和文档同时一致时才算完成。发现失败、污染比较或未支持语义时，保留它们作为交付证据，不通过缩小测试范围让结果变绿。
