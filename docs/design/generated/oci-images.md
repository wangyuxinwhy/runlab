---
title: "OCI Image 与本地 Catalog"
description: "定义 RunLab 的 OCI Image 内容身份、Store、Catalog、验证、保留与传输边界。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# OCI Image 与本地 Catalog

本页定义 RunLab 如何保存、验证和发现 OCI Image。Run Protocol 只要求 `RunInput` 和 `RunOutput` 使用确切的 OCI Image Descriptor。名称解析、内容保留和传输由 RunLab 实现。

## Store 与 Catalog

```text
Local Image Catalog: mutable name/tag → Manifest Descriptor
OCI Image Store:     immutable digest → 确切内容字节
```

Catalog 帮助调用方按名称发现 Image。Store 保存 Manifest、Image Config 和 Layers。名称更新后，已经构造完成的 `RunInput` 仍然引用原来的 Descriptor。

Catalog Entry 还可以保存用于选择和理解 Image 的本地 metadata：

```json
{
  "description": "Python 3.12 + uv，适合作为 coding agent 基础环境",
  "labels": {
    "python": "3.12",
    "runtime": "python"
  }
}
```

`description` 是可选的简短自然语言说明。`labels` 是调用方提供的任意字符串键值对，RunLab 不预定义或解释 key 的领域含义。metadata 只是选择提示，不是 RunLab 验证过的 Image 能力声明；平台、Descriptor 和 OCI 内容仍由相应的结构化事实表达。

Catalog metadata 属于可变名称映射，不属于 OCI Image 内容。同一个 Manifest 可以由多个名称引用并分别携带不同 metadata；更新 metadata 不改变 Manifest `digest`。OCI Image 自身已有的标准 annotations 仍作为 Image 内容中的固有信息读取，RunLab 不把本地 metadata 写回 Manifest、Image Config 或 Descriptor。

本地 Catalog 只维护当前 RunLab 实例中的名称映射。跨机器发现和分发 OCI Image 时，使用标准 OCI Registry 或显式传输 OCI 内容。

## Manifest `digest` 是 Image 身份

OCI Image Manifest 通过 Descriptor 绑定一份 Image Config 和有序 Layers。Manifest `digest` 因而标识一份确切的完整 OCI Image。

同一目录树使用不同 Layer 划分、压缩方式或 Image Config，会形成不同 Manifest 字节和不同 `digest`。`digest` 相同可以证明确切 Image 相同。`digest` 不同不能单独证明展开后的文件系统不同。

## 从选择器构造 `RunInput`

RunLab 的调用接口可以接受 Catalog 名称、`tag`、`digest` 或远端 `reference`。把请求交给 Run Engine 前，RunLab 必须：

1. 把选择器解析成具体 Manifest Descriptor。
2. 取得 Manifest、Image Config 和全部 Layers。
3. 校验每个 Descriptor 的 `digest` 与 `size`。
4. 校验 Image 结构、DiffID、`platform` 与执行能力的相容性。
5. 把解析后的 Descriptor 写入 `RunInput.initial_environment`。

任何必需的内容缺失、大小不符或 `digest` 校验失败时，RunLab 都不能构造可执行的 `RunInput`。原始 `reference` 可以作为来源信息另行保存，但不代替协议输入中的 Descriptor。

## 内容进入本地 Store

Image 可以从标准 OCI Layout、受支持的归档文件或外部 OCI Distribution Registry 导入。RunLab 必须先逐项验证 Descriptor、内容大小和 `digest`，再按 `digest` 写入本地 Store。

已经存在相同 `digest` 的对象必须保持原有字节不变。全部内容发布成功后，才能更新 Catalog 名称。检查、展开、比较和导出 Image 时，RunLab 直接读取 Store 中经过验证的 OCI 内容。

## 最终环境仍是普通 OCI Image

Run Engine 返回的最终环境是标准 OCI Image Descriptor。相应 Manifest、Image Config 和 Layers 写入同一 Store 后，这份 Image 可以被检查、导出、命名，也可以成为后续 `RunInput` 的初始环境。

最终 Image 的内容语义由[Run 的初始环境与最终环境](/design/generated/run-environments)定义。执行机制只提供停止后的受控文件系统状态，不能绕过 OCI 验证直接发布临时目录或 Runtime 私有对象。

## 保留与垃圾回收

Catalog 名称、持久 Run Record 和正在执行的资源共同形成内容保留根。内容回收必须先生成可审查计划，再在排他维护边界内应用，并在删除前重新确认对象仍未被任何保留根引用。引用图不完整时，Apply 必须 fail closed，在删除任何 OCI blob、snapshot chain 或 staging entry 之前失败。snapshot cache 是可重建的执行缓存，但清除会使之后的启动计时成为冷缓存证据，调用方必须显式报告。

`reference_graph_complete` 只声明保留图足以安全判定可达性，不是全库内容 scrub。Manifest 和 Image Config 必须按 Descriptor 逐字节校验并解析，因为它们拥有引用边和 snapshot chain 所需事实；Layer 默认只检查本地对象是常规文件且 size 与 Descriptor 一致。Layer 即使内容损坏也仍按其 Descriptor 引用保留，因此 `storage status` 和 `storage prune check` 不为每次图检查全量重算全部 Layer digest；逐字节内容校验属于内容导入与使用边界。

终态 Run Asset 可以通过独立的 checked deletion workflow 永久删除。调用方先冻结最多 1000 个确切 `run_id`；`run delete check` 只读取数据库事实，逐条返回终态时间、record fingerprint、逻辑记录字节，以及所有 Program Final Image 仍被哪些 Catalog 名称引用。Catalog 引用不阻止 Run 删除：内容仍由 Catalog 保留，但删除会断开该 Image 与这次执行事实的追溯链。

当回收收益影响删除决策时，调用方必须在不可逆删除前使用 `storage prune check --without-runs FILE`。它在 Storage 边界内假设这些终态 Run 不再作为保留根，计算 OCI blob 和 snapshot chain 的边际不可达量，但不修改任何 State。Run 删除提交后，实际内容仍由普通 `storage prune check|apply` 独立处理；数据库事务与文件系统删除不组成跨介质原子事务。

只读命令不能顺手下载、修复、删除或重新命名内容。远端 Registry 是否仍保留某个 `digest`，也不属于本地 Run Asset 的隐式保证。

## 责任边界

Catalog 负责名称到 Descriptor 的映射，OCI Image Store 负责 Descriptor 所标识的内容字节，Run Engine 负责使用初始 Image 并产生最终 Image，RunLab Storage 负责持久 Run 对内容的引用。宿主路径和外部 `bind mount` 不能代替 OCI Image 的内容保存与传输。
