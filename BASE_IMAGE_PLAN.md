# RunLab Agent Image Catalog 构建方案

本文是 `base`、`pi`、`claude`、`codex` 与 `all` 的本地实施方案。它不定义新的 RunLab 协议或 Image Builder；Docker Buildx 负责构建标准 OCI Image，RunLab 只负责导入、命名、运行和读取。

## 目标与边界

`base` 是通用 Agent 工作台，而不是某个 Agent、项目或 benchmark 的运行环境。首版目标平台是当前 macOS Managed VM 对应的 `linux/arm64`。

它必须满足：

- Python、uv、Node.js、npm、Git、JSON 和常见 Linux 调试/构建工具拿来即用。
- 默认以普通用户运行，`HOME`、工作区和产物目录均可写。
- `pi`、`claude`、`codex` 和 `all` 都以完全相同的 `base` Layer chain 为前缀。
- 不包含 Agent CLI、模型凭据、SWE-bench 仓库、任务内容或依赖缓存。
- 不依赖 RunLab 专用 build DSL，也不要求 RunLab 理解 Dockerfile。

首版明确不做：

- 不同时发布 `linux/amd64`。同一 Dockerfile 必须保留可增加该平台的能力，但先用真实的 arm64 Managed VM 验证。
- 不预装 Rust、Go、Java、浏览器或数据库服务。真实任务证明需要后，再作为派生 Image 或 base 清单变更加入。
- 不为 apt 引入 snapshot repository 或逐包版本锁。上游基础 Image、Node 和 uv 精确锁定；apt 在一次构建中解析出的结果由最终 OCI digest 固定。需要更新时重新构建并得到新 digest，不假装相同 Dockerfile 必然产生相同 Image。
- 不在 Image 中保存 build cache、下载缓存、凭据或宿主配置。

## 构建输入锁

首个构建采用以下输入：

| 输入 | 锁定值 | 理由 |
| --- | --- | --- |
| Ubuntu | `ubuntu:24.04@sha256:33ceb71981b602c1a7443a53469e4dba065f7503eab3078a2d7a57a2ab987517` | LTS，当前 Managed VM 也采用 Ubuntu 24.04；digest 锁定完整多平台 Image Index。 |
| Node.js | `24.20.0` | 当前 LTS；使用 nodejs.org 官方二进制，不额外引入第三方 apt repository。 |
| Node arm64 tar | `sha256:5f4ddab610c1ab2016b3c227cebdbf6d9495161487e4739c7b90090595f465f7` | 构建时校验 `node-v24.20.0-linux-arm64.tar.xz` 的精确字节。 |
| Node x64 tar | `sha256:2f2c0da162318f0de47665410c7c8c2ed3d36c8f3105de4bbc61176c70a7cbf2` | 为后续 amd64 保留同一构建定义，不代表首版已经发布 amd64。 |
| Python | Ubuntu 24.04 的 Python 3.12 系列 | 与系统 C library 和 apt 开发包自然一致；具体 patch 版本在构建验收时记录。 |
| uv | `ghcr.io/astral-sh/uv:0.12.7@sha256:95f2aa1fe59274951cfe9b0cbc7972e879ff1004bc8945d130a32eb0dbd85945` | 从官方 distroless Image 复制 `uv`/`uvx`，版本和 Image digest 都固定。 |

这些值直接保存在 Dockerfile 中，避免一份无人执行的第二份 lock 文件。升级任何一项必须是显式源码 diff，并重新完成全部 smoke verification。

版本与构建机制分别以 [Ubuntu Official Image](https://hub.docker.com/_/ubuntu)、[Node.js Releases](https://nodejs.org/en/about/previous-releases)、[uv Docker integration](https://docs.astral.sh/uv/guides/integration/docker/) 和 [Docker OCI exporter](https://docs.docker.com/build/exporters/oci-docker/) 为外部来源；精确 digest 和 checksum 已在当前环境重新解析、校验。

## 物理 Layer 划分

Layer 按“变化频率和责任”划分，不按每个软件拆成碎片：

| Layer | 内容 | 变化原因 |
| --- | --- | --- |
| L0 | digest-pinned Ubuntu 24.04 rootfs | Ubuntu base 更新。 |
| L1 | 通用系统、网络、VCS、诊断和 native build 工具 | apt 软件清单或基础安全更新。 |
| L2 | Python 3.12 runtime、venv、pip 和开发头文件 | Python/Ubuntu 包更新。 |
| L3 | Node.js 24.20.0、npm、npx | Node LTS patch 更新。 |
| L4 | uv 0.12.7 和 uvx | uv 更新。 |
| L5 | `agent` 用户、标准目录、`fd` shim | 用户或目录契约更新。 |

OCI Image Config 是整份 Image 的配置，不属于某个 filesystem Layer。Dockerfile 中的 `ENV`、`USER`、`WORKDIR` 和 `CMD` 可以产生 history entry，但不应产生有内容的 Layer。

当前 Buildx OCI exporter 为 `WORKDIR /workspace` 追加一个 32-byte 的 canonical empty Layer。因此实际 Manifest 包含 L0-L5 六个内容 Layer，外加一个空 Layer；它不改变展开后的文件系统，不值得为消除它增加 Image Config 后处理。

所有派生 Agent Image 都共享 L0-L5 的完全相同 DiffID chain。三个独立 Agent target 从 `base` 派生；`all` 从 `pi` 派生，再复用另外两个构建 stage 的安装内容，因此还共享完整 Pi Layer：

```text
base  ──┬── pi ── all (+ claude + codex)
        ├── claude
        └── codex
```

不为让 `all` 与另外两个兄弟分支拥有相同 Layer descriptor 而引入自定义 OCI assembler。BuildKit 会复用它们的构建结果，RunLab Store 与 NativeEngine snapshot cache 则复用共同的 base 和 Pi prefix；这覆盖主要收益，同时保持普通 Dockerfile 心智模型。

任务 Image 继续在 Agent Image 之上分层：

```text
base → pi/all → repository snapshot → task instance
```

仓库和任务不得进入 base。任务构建使用 `COPY --chown=1000:1000`。Run 产生的文件变化仍进入最终 Image 的最新 Layer；`filesystem get` 读取 `/artifacts/...` 时会优先命中该最新 Layer，不需要为了读取速度扭曲 base 的 Layer 结构。

## 软件清单

L1 的 apt 清单固定为：

```text
autoconf
automake
bash
bzip2
ca-certificates
cmake
coreutils
curl
diffutils
dnsutils
fd-find
file
findutils
gawk
git
git-lfs
grep
gzip
iproute2
iputils-ping
jq
less
libffi-dev
libssl-dev
libtool
lsof
netcat-openbsd
ninja-build
openssh-client
patch
pkg-config
procps
psmisc
ripgrep
rsync
sed
sqlite3
strace
tar
tree
unzip
wget
xz-utils
zip
zstd
build-essential
```

L2 的 apt 清单固定为：

```text
python3
python3-dev
python3-pip
python3-venv
python-is-python3
```

不预装任何第三方 Python package 或全局 npm package。项目依赖属于派生 Image 或 Run 内的工作区；Agent CLI 属于 `pi`、`claude`、`codex` Layer。`sudo`、Docker CLI 和 systemd 不进入 base，因为当前 Run 的 capability/profile 不承诺它们可用。

构建使用 BuildKit cache mount 保存 apt 下载与索引，但这些缓存不得进入最终 Layer。每个 apt Layer 在同一个 `RUN` 内完成 update、install 和清理。

## 用户与目录契约

Ubuntu 24.04 的锁定 Image 已包含 `ubuntu:1000:1000`。base 将其原地重命名并迁移 HOME，不创建第二个相同数字身份。最终唯一的普通用户契约是：

```text
user:  agent
uid:   1000
group: agent
gid:   1000
shell: /bin/bash
home:  /home/agent
```

目录约定为：

| 路径 | owner | mode | 用途 |
| --- | --- | --- | --- |
| `/home/agent` | `1000:1000` | `0755` | 用户 HOME。 |
| `/home/agent/.cache` | `1000:1000` | `0755` | uv、npm 和其他用户级缓存。base 中保持为空。 |
| `/home/agent/.config` | `1000:1000` | `0755` | 用户配置。凭据不烘焙到这里。 |
| `/home/agent/.local/bin` | `1000:1000` | `0755` | Run 内允许的用户级可执行文件。 |
| `/home/agent/.local/share` | `1000:1000` | `0755` | XDG data。 |
| `/home/agent/.local/state` | `1000:1000` | `0755` | XDG state。 |
| `/workspace` | `1000:1000` | `0755` | 默认工作目录；base 中为空。 |
| `/artifacts` | `1000:1000` | `0755` | Agent 应显式写入、之后由 `filesystem get` 读取的产物。 |
| `/opt/agents` | `0:0` | `0755` | 派生 Agent Image 的只读安装前缀；base 中为空。 |

L5 同时创建 `/usr/local/bin/fd -> /usr/bin/fdfind`。除此之外不修改 shell rc、不设置 Git identity、不添加 `safe.directory=*`，也不创建默认凭据文件。

## OCI Image Config

base 的有效 Image Config 必须等价于：

```json
{
  "User": "1000:1000",
  "Env": [
    "HOME=/home/agent",
    "LANG=C.UTF-8",
    "LC_ALL=C.UTF-8",
    "TERM=xterm",
    "PATH=/home/agent/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    "XDG_CACHE_HOME=/home/agent/.cache",
    "XDG_CONFIG_HOME=/home/agent/.config",
    "XDG_DATA_HOME=/home/agent/.local/share",
    "XDG_STATE_HOME=/home/agent/.local/state",
    "NPM_CONFIG_PREFIX=/home/agent/.local"
  ],
  "WorkingDir": "/workspace",
  "Cmd": ["/bin/bash"]
}
```

`Entrypoint` 故意省略。base 是通用工作台，不替 Agent 或任务决定主程序；派生 Agent Image 再声明自己的 `Entrypoint`/`Cmd`。`Volumes`、`ExposedPorts` 和 `Healthcheck` 也省略，因为它们没有进入当前 RunLab 的执行契约。

`User` 必须使用数字 `uid:gid`。当前 `run config generate` 不做容器内用户名解析，只接受 root 或数字身份。Image Config 中只出现上述普通环境变量，Secret 仍通过 `run start --secret-env` 或 `--secret-file` 进入 Run Protocol。

## 构建源与命令

实施源位于以下普通外部构建文件：

```text
images/
├── Dockerfile
├── packages.system.txt
├── packages.python.txt
├── smoke-base.sh
├── smoke-pi.sh
├── smoke-claude.sh
├── smoke-codex.sh
└── smoke-all.sh
```

Dockerfile 使用 `base`、`pi`、`claude`、`codex` 和 `all` target。package 清单通过 BuildKit bind mount 读取，不作为文件复制进最终 Image。Node 下载按 `TARGETARCH` 选择 `arm64` 或 `x64` checksum；未知架构立即失败。uv 通过 digest-pinned official Image stage 复制。

首个构建只输出 OCI archive，不同时 `--load` 到 Docker，避免在 Docker image store 和导出文件中额外保留一份完整 Image：

```bash
mkdir -p dist

docker buildx build \
  --platform linux/arm64 \
  --target base \
  --provenance=false \
  --sbom=false \
  --output type=oci,dest=dist/runlab-base-linux-arm64.oci.tar \
  --file images/Dockerfile \
  images
```

`--provenance=false --sbom=false` 是因为当前 RunLab import 只接受包含一个 Image Manifest 的 OCI Layout；不让额外 attestation manifest 偶然改变导入形状。Buildx 仍然只是外部 Builder。

导入 Catalog：

```bash
runlab image import dist/runlab-base-linux-arm64.oci.tar \
  --name base \
  --description "Ubuntu 24.04 Agent workbench with Python 3.12, uv 0.12.7, Node.js 24 LTS, npm, Git and native build tools" \
  --label role=base \
  --label os=ubuntu-24.04 \
  --label python=3.12 \
  --label uv=0.12.7 \
  --label node=24.20.0
```

Catalog metadata 是选择提示；实际能力由 OCI 内容和 smoke verification 证明。导入并验证 Store 内容后，如果该 archive 不作为分发物保留，应删除 `dist/runlab-base-linux-arm64.oci.tar`，避免与 RunLab Store 长期重复占用磁盘。执行删除前先确认 Catalog 已解析到预期 Manifest digest。

## 派生 Agent Image

三个 Agent CLI 都使用精确版本，安装到 root-owned `/opt/agents/<agent>`。npm 下载目录和 Node compile cache 使用 BuildKit cache mount，不进入 Layer；运行时状态目录由 Agent 用户拥有，凭据仍只能在 Run 时通过 Secret 注入：

| Catalog 名称 | 版本 | 可写状态目录 | 默认程序 |
| --- | --- | --- | --- |
| `pi` | Pi `0.84.3` | `/home/agent/.pi/agent` | `pi --approve` |
| `claude` | Claude Code `2.1.250` | `/home/agent/.claude` | `claude --print --dangerously-skip-permissions` |
| `codex` | Codex CLI `0.150.1` | `/home/agent/.codex` | `codex exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -` |
| `all` | 上述三者 | 上述三者 | `/bin/bash` |

这些 permission bypass 只移除 Agent CLI 在 Run 内的二次确认和内建 sandbox；外部隔离仍由 RunLab 的 NativeEngine、OCI Runtime Configuration、namespace、Secret 与 Network control 提供。Image 不预装登录态，也不固定 Provider 或模型。

### Pi

`pi` 直接 `FROM base`，只增加一个内容 Layer。`@earendil-works/pi-coding-agent` 精确锁定为 `0.84.3`，安装到 root-owned `/opt/agents/pi`；发布包自带 `npm-shrinkwrap.json`，无需维护第二份依赖锁。npm 下载目录和 Node compile cache 都使用 BuildKit cache mount，不进入 Layer。`/usr/local/bin/pi` 是指向隔离安装前缀的符号链接，因此普通 shell 也能直接发现 Pi，普通用户不能修改安装内容。

pi Image Config 在 base 之上只覆盖：

```json
{
  "Entrypoint": ["/opt/agents/pi/bin/pi"],
  "Cmd": ["--approve"]
}
```

Image 不固定 Provider、模型或凭据。Pi 自己把 piped stdin 解释为非交互 print mode，因此 `run start --stdin ...` 不需要 RunLab wrapper；需要指定模型时，调用者用公开的 Runtime Configuration JSON 能力追加普通 Pi argv。例如：

```bash
runlab run config generate --image pi \
  | jq '.process.args += ["--provider", "deepseek", "--model", "deepseek-v4-flash"]' \
  >pi-deepseek.json

runlab run start \
  --id 550e8400-e29b-41d4-a716-446655440000 \
  --image pi \
  --runtime-config pi-deepseek.json \
  --stdin task.md \
  --secret-env DEEPSEEK_API_KEY \
  --network egress
```

这不是新的 RunLab Agent DSL：Provider、模型和其他开关仍是 Pi 自己的 argv；Secret 与 network 仍分别属于 Run Protocol 输入和 Run Control。

构建和导入采用与 base 相同的 OCI 流程，只把 target 与 Catalog metadata 改为 pi：

```bash
docker buildx build \
  --platform linux/arm64 \
  --target pi \
  --provenance=false \
  --sbom=false \
  --output type=oci,dest=dist/runlab-pi-linux-arm64.oci.tar \
  --file images/Dockerfile \
  images

runlab image import dist/runlab-pi-linux-arm64.oci.tar \
  --name pi \
  --description "Pi 0.84.3 Agent runtime on RunLab base" \
  --label role=agent \
  --label agent=pi \
  --label pi=0.84.3 \
  --label base=runlab-base
```

### Claude Code

`claude` 直接从 base 派生，安装 `@anthropic-ai/claude-code@2.1.250`，并提供 `/usr/local/bin/claude` symlink。默认 `--print` 让 stdin 任务成为一次非交互调用；调用方仍可用 Runtime Configuration JSON 改写任意 Claude argv。宿主当前 Claude 订阅态保存在 macOS Keychain，没有可移植的 Secret file，因此当前只承诺并验证 CLI 离线可执行性，不把宿主登录态复制进 Image。

### Codex CLI

`codex` 直接从 base 派生，安装 `@openai/codex@0.150.1`，并提供 `/usr/local/bin/codex` symlink。默认 `exec ... -` 从 stdin 接受任务。构建时的 Codex 状态目录使用 tmpfs，避免版本检查产生的 `tmp/arg0` 进入 Layer；随后用独立的 146-byte Layer 创建 `1000:1000`、mode `0700` 的空状态目录。

订阅登录通过 Run Secret file 交付，例如：

```bash
runlab run start \
  --id 550e8400-e29b-41d4-a716-446655440000 \
  --image codex \
  --stdin task.md \
  --secret-file "$HOME/.codex/auth.json=/home/agent/.codex/auth.json" \
  --network egress
```

Secret file 在 Program 整个执行期间保持只读可见，在 Final Environment 捕获前移除。仅设置 `OPENAI_API_KEY` 并不等价于当前 Codex CLI 已完成登录；若调用方使用 API key，应先走 Codex 自己公开的登录流程产生它理解的认证状态。

### All-in-one

`all` 从 `pi` 派生，并从 Claude 与 Codex stage 复制各自的 `/opt/agents` 安装。它用 PATH 直接暴露三个 CLI，不再复制各自的 `/usr/local/bin` symlink；默认 `/bin/bash` 避免替用户猜测要调用哪个 Agent。任务 Image 可直接 `FROM all`，仍保持 `base → pi → all → repository → task` 的缓存前缀。

## 验收

`images/smoke-base.sh` 必须在一次真实 Run 中检查并以非零状态报告任何失败：

1. `id -u` 和 `id -g` 都是 `1000`，`HOME=/home/agent`，`pwd=/workspace`。
2. `/home/agent`、`/workspace`、`/artifacts` 可写，`/opt/agents` 对普通用户不可写。
3. `python` 与 `python3` 均为 Python 3.12 系列。
4. `uv --version` 为 `0.12.7`。
5. `node --version` 为 `v24.20.0`，`npm` 和 `npx` 可执行。
6. L1/L2 清单中的每个命令或 package 都存在；`git lfs version` 可执行。
7. Image 中不存在已填充的 Agent credential、npm/uv 下载 cache、SWE-bench 仓库或任务文件。

每个派生 Image 的 smoke 另外验证精确 CLI 版本、PATH、root-owned 安装不可写、Agent 状态目录可写、关键非交互参数可发现，并把结果写入 `/artifacts/<agent>-smoke.json`。`all` 同时检查三个 CLI。所有 smoke 必须通过真实 `runlab run start` 执行，产物必须通过 `runlab filesystem get` 取回。

验收顺序：

```text
Buildx 构建 OCI archive
  → runlab image import
  → runlab image get base 检查 platform/config/layers
  → runlab run config generate --image base 检查生成配置
  → isolated 真实 Run 执行 smoke-base.sh
  → filesystem get 读取 smoke 产物
  → 再跑一次 /bin/true 记录 cold/warm 启动事实
```

首次 cold/warm 时间和磁盘占用只记录为工程事实，不在看到结果前设一个方便通过的阈值。功能门禁是输入身份、Image Config、工具版本、目录权限、真实 NativeEngine 执行和产物可取回全部正确。

## 实施切片

本方案按三个独立切片推进：

1. 添加 `images/` 构建源，并在 Docker 中构建、运行 smoke。
2. 输出单 Manifest OCI archive，导入 RunLab Catalog，在真实 Managed VM 完成 Run 与 `filesystem get`。
3. 记录实际 Manifest digest、各 Layer 大小、展开后大小、cold/warm 时间和发现的问题；确认稳定后，再把长期有效的 Image Catalog 设计结论写入 Agent Wiki。

五个 target 与对应 smoke 已实现并导入当前 Managed VM Catalog。后续任务 Image 直接选择合适 Agent Image 作为父层；不为每个任务重新安装这些 CLI。
