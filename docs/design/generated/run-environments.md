---
title: "Run 的初始环境与最终环境"
description: "定义 RunInput 中的初始 OCI Image 与 RunOutput 中的最终 OCI Image。"
---

<!-- Generated design snapshot. Do not edit directly. -->

# Run 的初始环境与最终环境

本页定义每个 Program 的 `initial_environment` 和 `final_environment`。初始环境使用标准 OCI Image 表达；调用方请求捕获时，最终环境也使用 OCI Image 表达。未请求或无法捕获时，最终环境保存相应状态而不是 Image。

`initial_environment` 属于 `RunInput`，`final_environment` 属于 `RunOutput`。

## 同一种对象表达执行前后

```text
initial_environment
（OCI Image Descriptor）
          │
          │ 验证并展开
          ▼
Program 的私有可写 rootfs
          │
          │ 执行、停止、移除运行时文件系统
          ▼
停止后的受控 rootfs
          ├── capture_final_environment = true
          │        │
          │        │ 捕获为完整 OCI Image
          │        ▼
          │   Descriptor 或不可取得原因
          │
          └── capture_final_environment = false
                   │
                   ▼
              not_requested
```

OCI Image 表达持久文件系统及其 Image Config。要启动的进程、参数、用户、工作目录和其他本次执行条件由 OCI Runtime Configuration 决定。两者不能互相替代。

## 初始环境

`initial_environment` 是指向 [OCI Image Manifest 1.1.1](https://github.com/opencontainers/image-spec/blob/v1.1.1/manifest.md) 的 Descriptor。Manifest 通过 Descriptor 引用一份 Image Config 和一组有序 Layers。按 OCI 规则应用这些 Layers，必须得到 Program 启动前的根文件系统。

引擎必须验证 Manifest、Image Config、Layers、`rootfs.diff_ids` 和平台兼容性。Descriptor 的 `digest` 标识确切 OCI Image 字节，不标识抽象文件树。两份 Image 即使展开后相同，只要 Manifest、Config、Layer 划分、压缩或确切字节不同，就拥有不同的内容身份。

## 私有可写 `rootfs`

每个 Program 必须从自己的初始环境得到一份新的私有可写 `rootfs`。Program 可以修改它，但不能修改 OCI 内容源，也不能把修改泄漏给其他 Program 或其他调用。

`runtime_config.root.path` 使用字面值 `rootfs`。执行时，引擎把初始环境物化到当前 `bundle` 的这个位置。`bundle` 绝对路径、物化目录和 Runtime 对象名都是内部临时资源。

## 运行时文件系统不属于环境

`bind mount`、`proc`、`tmpfs`、`devpts` 和其他运行时文件系统会影响 Program 执行时看到的内容，但不属于初始环境，也不进入最终环境。

结果收集时，引擎必须停止所有可能继续写入 `rootfs` 的进程，并移除覆盖 `rootfs` 路径的运行时文件系统。请求捕获时，这些操作必须在捕获前完成；未请求时，它们仍属于调用内清理。因此：

- `bind mount` 的外部内容不进入最终环境。
- 对可写 `bind mount` 的修改留在外部文件系统。
- 临时运行时文件系统的内容不进入最终环境。
- `mount` 移除后，原来被遮盖的路径按照 `rootfs` 自身留下的状态捕获。

请求捕获时，如果无法安全停止进程、移除运行时文件系统或证明 `rootfs` 已稳定，`final_environment` 必须明确标记为不可取得。未请求捕获时，`final_environment` 仍为 `not_requested`，相关失败由进程、停止动作或操作错误表达。

## 最终环境

`RunInput.controls.capture_final_environment` 为假时，引擎不得构造或发布最终 OCI Image，并把 `final_environment` 标记为 `not_requested`。这不会跳过执行、结果收集或临时资源清理。

请求捕获且可用的 `final_environment` 必须是指向完整 OCI Image Manifest 的 Descriptor。该 Image 在脱离当前 `bundle`、临时目录和 Runtime 对象后，必须仍能独立验证和展开，并可作为后续调用的 `initial_environment`。

Program 非零退出、超时、取消或被强制终止，不影响这一要求。只要 `rootfs` 已稳定且可以忠实捕获，引擎就返回实际得到的最终 Image。Program 没有改变 `rootfs` 时，可以复用初始环境的 Descriptor。

捕获失败时必须返回不可取得原因，不能使用初始环境、单独 Layer、本地目录、部分 Image 或空 Image 冒充最终环境。

## 最终 Image Config

最终 Image Config 中描述默认执行方式的 `config` 字段必须与初始环境保持一致。本次执行的参数、环境变量、用户、退出结果和停止信号属于 `RunInput` 或 `RunOutput`，不能写回 Image Config，除非它们本来就是 `rootfs` 中的文件内容。

平台字段必须继续描述同一 Program 环境，`rootfs.diff_ids` 必须与最终 Layers 对应。`history` 等 OCI 元数据必须符合 OCI Image 规范，但不代替执行事实。

## 表达相同 `rootfs` 的方式

实现可以复用初始 Layers 并追加一个表示变化的 Layer，也可以用另一组有效 Layers 表达相同的最终 `rootfs`。协议不规定 Layer 数量、压缩格式、文件顺序或内部去重方式。

引擎必须验证新 Manifest、Image Config 和全部 Layers，并证明展开结果等于捕获边界上的 `rootfs`。不同实现可能为相同 `rootfs` 生成不同的 Manifest `digest`。`digest` 不同只证明 OCI Image 对象不同，不能单独证明展开后的文件系统不同。
