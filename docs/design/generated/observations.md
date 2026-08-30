---
title: "RunLab Run Observation"
description: "定义 terminal Run 上持久 typed Observation 的 Type Registry、Method、通用 payload、提交、修正、撤回、查询与删除边界。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# RunLab Run Observation

RunLab 允许在一条 Run terminal 之后继续追加结构化观测结果，而不改写已经封口的 Run Record。Observation 是 RunLab 产品层的数据资产，不是 Run Protocol、RunOutput 或 RunEngine 的组成部分。

[Run Protocol](/design/generated/run-protocol)只负责一次执行形成可信的 `RunOutput` 或 `EngineError`；[RunLab Run Live Event 与 CLI 输出](/design/generated/live-events)只负责执行期间的非持久进度旁路。Observation 专指 terminal Run 上持久、typed、Method-attributed 的追加记录。

## 核心对象与职责

| 对象 | 责任 |
| --- | --- |
| Observation Type | 以 description 和 payload Schema 定义跨 Method 稳定的语义契约。 |
| Method | 从自己选择和理解的来源产生某个 Type payload，并声明 name 与 version。 |
| Observation | 一个 Method 对一条 terminal Run 产生并显式提交的不可变实例。 |
| Retraction | 说明一个 active Observation 不应再作为当前结论使用的不可变记录。 |

一条 Observation 只有一个 `run_id` 和一个 versioned Type。不同 Method 可以对同一 Run 产生同一 Type 的 Observation；RunLab 不自动合并、投票、选优或把 Observation 改写成 Run Record 事实。零条 Observation 不影响 Run 的协议完整性。

Method 自己负责来源发现、来源格式解析、语义推导、coverage 判断以及是否有足够结果可以提交。RunLab 信任调用方声明的 Method 和 payload，不试图登记或复原 Method 的全部输入，也不保存不可能完整的 `source_refs`。RunLab 不知道具体 Method 的可执行文件、库、prompt、workflow 或中间报告 Schema，也不自动发现、触发、调度或重试 Method。

这条边界不要求给 Run Record 增加通用 `Context`。Observation 通过 `run_id` 以一条 Run 为主体；Method 自己拥有的输入与处理上下文留在 Method 及其调用工作流中。

## Observation Type Registry

每个 Observation Type 是 create-only 的不可变定义，固定只有五个字段：

```json
{
  "schema_version": 1,
  "type": "example/rubric_score@v1",
  "title": "Rubric score",
  "description": "A score from zero through one produced by the declared rubric Method.",
  "payload_schema": {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "type": "object",
    "additionalProperties": false,
    "required": ["score"],
    "properties": {
      "score": {"type": "number", "minimum": 0, "maximum": 1}
    }
  }
}
```

`description` 是一个 string，承载生产者和消费者都必须遵守的完整语义契约；不再拆出结构化 `contract` 子字段。`payload_schema` 是自包含的 JSON Schema Draft 2020-12 object，RunLab 在离线模式编译，不依赖网络获取外部引用。

Type identity 使用 `namespace/name@vN`。相同定义的重复注册幂等；同一 identity 注册不同定义是 Conflict。`runlab/` namespace 保留给内置 Type。公开命令为：

```text
runlab observation type register --document TYPE.json
runlab observation type get TYPE
runlab observation type list
```

内置与外部注册 Type 在运行时一视同仁：它们都是同一个 Registry 的不可变记录，共用同一个 JSON Schema validator、Observation table、修正规则和 Query Plane。RunLab 不为某个内置 Type 增加 Rust typed payload validator、专用存储列或专用 Relation。

## 提交文档

Observation 提交文档 schema version 1：

```json
{
  "schema_version": 1,
  "observation_id": "canonical-lowercase-uuid-v4",
  "run_id": "canonical-lowercase-uuid-v4",
  "type": "example/rubric_score@v1",
  "method": {"name": "example/rubric-grader", "version": "1.0.0"},
  "payload": {"score": 0.8}
}
```

输入没有 `kind`。CLI command 已经区分 Type definition、Observation 与 Retraction，`schema_version` 负责各自文档结构的版本，因此再加入固定 kind 只会重复表达同一事实。

RunLab 只接受已经发布 terminal completion 的 Run，以及已经注册的 Type。提交时先用该 Type 的 `payload_schema` 验证 payload，再执行 append-only 持久化。RunLab 不重新执行 Method，也不把 payload 的正确性提升成执行事实。

本地单机 State 当前没有认证的多用户 principal，因此不伪造 `submitted_by`。持久记录保存 RunLab 首次接受的 `submitted_at`、Method name/version 和完整 payload；Method identity 说明转换实现，不冒充调用者身份。未来只有在产品引入真实认证主体后才定义 submitter。

调用方拥有 canonical UUID v4 `observation_id`。同一 identity 与相同语义内容的重试返回既有记录；同一 identity 绑定不同内容是 Conflict。`submitted_at` 由首次成功提交产生，不由输入指定。

## 内置 Token Usage Type

`runlab/token_usage@v1` 是预注册的普通 Type，而不是硬编码的特殊提交路径。它表达一个 Method 归因到一条 Run 的累计 Agent token usage：

```json
{
  "coverage": "complete",
  "input_tokens": 12000,
  "cached_input_tokens": 8000,
  "cache_write_input_tokens": null,
  "output_tokens": 3400,
  "reasoning_output_tokens": 900
}
```

- `input_tokens` 包含 ordinary input、cache reads 与 cache writes；`cached_input_tokens` 和 `cache_write_input_tokens` 是可选的已知子集，已知时不得超过 input。
- `output_tokens` 包含 reasoning output；`reasoning_output_tokens` 是可选的已知子集，已知时不得超过 output。
- 三个子集字段必须出现；JSON `null` 表示 Method 无法报告该子集，不能用 0 代替未知。
- `coverage` 为 `complete` 或 `partial`。`complete` 表示 Method 已确定这条 Run 的累计 input 与 output coverage 完整；`partial` 表示报告值是可靠的已知下界。累计 input 或 output 完全 unavailable 时不提交这个 Type。
- total 由消费者计算为 `input_tokens + output_tokens`，不在 payload 中重复保存。

其中的子集关系、coverage 含义和不可提交条件属于 Type `description` 的语义契约。RunLab 对这个 Type 执行的结构验证仍然只来自 Registry 中同一份 Draft 2020-12 Schema，不增加 token-specific 分支。

## 修正与撤回

Observation 不原地修改或删除。修正使用一个新 Observation identity，并设置 `supersedes_observation_id`。前一条记录必须存在、仍为 active，并且 Run 与 Type 相同；一条记录只能被一个后继替代。旧记录的派生状态变成 `superseded`。

不提供替代结果但已知旧结果不应继续使用时，调用方提交：

```json
{
  "schema_version": 1,
  "retraction_id": "canonical-lowercase-uuid-v4",
  "observation_id": "canonical-lowercase-uuid-v4",
  "reason": "the Method configuration was later shown to be invalid"
}
```

Retraction 输入同样不需要 `kind`。Retraction identity 由调用方拥有并支持原文幂等重试。只有 active Observation 能被撤回；superseded 或已 retracted 的记录不能再接受新的 retraction。修正和撤回都保留完整历史，`active`、`superseded`、`retracted` 是从追加记录派生的当前状态，不是对旧行的事实改写。

## 查询与生命周期

公共 Query Plane 提供三个通用 Relations：

- `observation_types`：Type、注册时间、title、description 和完整 payload Schema。
- `observations`：Observation identity、Run、Type、提交时间、Method name/version、完整 payload，以及 correction/retraction 派生状态。
- `observation_retractions`：不可变 Retraction 事实。

`observations` 只暴露跨 Type 的共同列。官方 Type 与注册 Type 的 payload 都通过 SQLite `json_extract(payload, ...)` 查询；调用方需要当前结论时显式选择 `state = 'active'`。RunLab 不默认隐藏历史，也不创建 `token_usage_observations` 等专用 Relation。

Observation 和 Retraction 是所属 Run Asset 的一部分。Run 删除 check 必须冻结 Run Record 与整套 Observation 历史的共同 identity；check 后追加修正或撤回会使计划 stale。成功 apply 在一个事务中删除 Run Record 与 Observation 历史，并继续保留永久 Run tombstone。State-level Type Registry 不随单条 Run 删除。

## 非目标

当前设计不提供 Method daemon、plugin runtime、自动 pipeline、跨 Run 评价、价格推断、模型成本、排名、统计显著性或 Experiment 实体。需要理解多条 Run 才能形成的比较属于外部分析；Observation 只以一条 Run 为主体。
