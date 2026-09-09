# 独立 sh WASM：GitHub Actions 构建计划

状态：待实施工作流设计；本次仅编写文档，没有运行云端构建或发布产物。总体目标见 [项目目标](../project-goals.md)。

## 1. 现有流程的可复用部分与阻碍

| 文件 | 当前情况 | 处理要求 |
| --- | --- | --- |
| [wasm-build.yml](../../.github/workflows/wasm-build.yml) | 已构建 bundler/web/nodejs，但绑定 `mayflower-k8s-runners`，同一流程另建 Pyodide；无手动触发 | 改成独立 sh 工作流，提供 workflow_dispatch，使用本 fork 可用 runner |
| [release.yml](../../.github/workflows/release.yml) | 最终 Release 依赖 Pyodide、crates/npm/PyPI 与容器发布 | 单独建立 standalone release 链路，不继承上游密钥或包发布目标 |
| [ci.yml](../../.github/workflows/ci.yml) | 主 CI 同样使用上游 runner，外部 fork PR 被跳过 | 配套迁移核心检查；公开 PR 使用隔离的 GitHub-hosted runner，不能把跳过当通过 |
| [setup-rust action](../../.github/actions/setup-rust/action.yml) | 从 rust-toolchain.toml 读取版本，安装系统编译依赖 | 复用版本单一来源；评估 Ubuntu runner 是否需要现有 apt 安装步骤 |
| [rust-toolchain.toml](../../rust-toolchain.toml) | 固定 Rust 1.95.0，同时声明 unknown-unknown 和 emscripten | shell 主链路仅要求 unknown-unknown；移除默认 emscripten 安装需求或拆分 legacy 配置 |
| [Cargo.toml](../../Cargo.toml) | 已有独立 browser crate；Pyodide crates 被 workspace exclude | 不需要先删除 Python 源码才能构建 shell；避免全 workspace 命令带入无关服务端交付 |
| [standalone build.sh](../../e2e/standalone/build.sh) | wasm-pack 输出到 E2E fixture，含本地 macOS 工具路径 | 抽出可参数化独立构建入口，移除宿主特定路径假设 |
| [standalone E2E](../../e2e/standalone/package.json) | 已有 Playwright 套件与 package-lock.json | 使用锁文件安装；验证即将上传的产物，避免另建一份不同二进制 |

现有 wasm-build 的注释记录 GitHub-hosted runner 上发生过 worker 超时与 `WebAssembly.Table.grow()` 失败。因此迁移到 `ubuntu-24.04` 是待验证的实施选择，不是已验证修复。先以 Chromium 单 worker、有限并发运行，记录资源与错误；若仍失败，诊断编译/内存行为或选择本项目可用的 runner，不能删除 E2E 门禁掩盖问题。

## 2. 构建输入与交付结构

主目标 `wasm32-unknown-unknown`；`wasm-pack --target` 的 bundler/web/nodejs 指 JS 包装格式，三者都不是 WASI 程序，也不是三种桌面 CPU 架构产物。宿主加载时必须使用该目录中配套的 JS 与 WASM。[wasm-pack 官方构建说明](https://rustwasm.github.io/docs/wasm-pack/commands/build.html)。

保留 Rust 版本的单一配置来源，并固定 wasm-pack、wasm-opt/Binaryen、Node 和 JS 依赖。后续实现时选择并验证工具确切版本，将其写入受版本控制的配置与构建清单；不要直接把 `stable`、浮动 installer、未固定 apt 包版本称为可复现构建。Cargo 使用 `--locked`，JS 使用 `npm ci` 或已有 workspace 的冻结锁文件安装。

建议产物目录：

```text
wasmsh-standalone-<version>-<commit>/
  web/                  # WASM + ESM loader + declarations + package metadata
  nodejs/               # WASM + Node loader + declarations + package metadata
  bundler/              # WASM + bundler loader + declarations + package metadata
  host/                 # 实施后的独立宿主适配代码及声明
  LICENSE
  README.md             # 本产物的加载、能力和限制说明
  SUPPORTED.md
  VERSION
  build-manifest.json
  SHA256SUMS
```

`host/` 是后续接入层交付要求，当前仓库尚没有满足本设计的独立宿主包。M0 只能标记基础 shell 产物，不能宣称 external/live-clock 已具备。

build-manifest 至少包括版本、源 commit、构建 run ID、Rust/wasm-pack/wasm-opt/Node 版本、目标、特性与资源限制配置；SHA256SUMS 覆盖最终发布文件（清单自身除外）。报告 WASM 原始大小和 gzip 大小，首个成功基线之后再制定体积回归阈值。

禁止打包 `pyodide*`、`python_stdlib.zip`、CPython、`.whl`、micropip 等运行时资产。还要执行离线初始化并观察没有隐式下载；只检查文件名不足以证明没有运行时依赖。CI 上用于普通开发工具的 Python 与向用户交付 WASM Python 是不同问题。

## 3. 工作流图与触发规则

```text
PR / main push / workflow_dispatch
  -> validate-core
  -> build-standalone
  -> test-final-artifacts (Node + browser + host matrix)
  -> upload-tested-artifacts

version tag
  -> 同一构建与实物验证链路
  -> package + checksum
  -> standalone GitHub Release
```

PR 和 main 构建只使用 `contents: read`。Artifact 可作为 job 间传递媒介，但标记为内部未验产物；只有所有必要验证通过的结果才成为可交付产物。Release job 单独获得 `contents: write`，不得在 PR 流程发布。普通 PR 的验证不依赖仓库 secrets，不使用 `pull_request_target` 执行贡献者代码。

concurrency 按 workflow/ref 区分，可取消旧分支构建；正式版本发布不要因无关分支更新而被取消。每个 job 有明确 timeout；测试失败保留报告，但不能继续发布成功产物。

## 4. 各阶段必须做什么

### A. validate-core

- 格式与 lint；核心 Rust 单元/集成测试，以及与 shell 兼容性有关的 TOML suite。
- 复用 `wasmsh-runtime/tests/runtime_protocol.rs` 的 external 用例、`shared_io.rs`、`wasmsh-utils` 的 net_types/date 测试。
- 新增回归对应 [需求矩阵](../design/ai-shell-requirements.md#6-验收矩阵)，避免以公共网络可用性作为核心 CI 前提。
- 核心测试列表显式覆盖依赖层；不要求 Pyodide、dispatcher 容器或 LLM API key。

### B. build-standalone

1. checkout 精确提交；安装受版本控制的固定工具版本，校验下载摘要。
2. 缓存只用于加速，key 包含 OS、目标、工具版本、Cargo.lock 与构建配置；缓存命中不能绕过源代码重建。
3. 用 `wasm-pack build crates/wasmsh-browser` 分别生成 web/nodejs/bundler 到 `pkg/`；沿用现有产物名，使用 release 与锁文件约束。
4. 明确由 wasm-pack 内置优化或独立固定版本 wasm-opt 中的一处负责最终优化，避免目前两次优化的隐含差异。所需 WASM feature flags 根据实际工具链验证。
5. 检查文件非空、WASM 魔数/可验证性、JS 与类型声明齐全；生成 manifest 和包清单。最终优化后才进入运行验证。

### C. test-final-artifacts

- Node 从产物目录加载，执行 `printf hello | wc -c`、文件写读、cwd/变量跨 Run 保存、退出状态与会话隔离。
- web 将同一已优化产物放入 fixture 运行 Playwright；不要在测试 job 又重新编译一次。检查初始化与 Worker 通信、基础 shell、真实宿主网络桥接。
- bundler 在最小锁定依赖的消费项目中实际打包并加载，防止只验证 Node 导致 ESM/package metadata 错误漏检。
- Windows/Linux/macOS 的 JS 宿主矩阵下载同一 WASM 包测试基础行为；external 使用小型跨平台测试程序制造 stdin、双输出、大流量、非零退出和取消场景。浏览器只验其声明支持的工具能力。
- 无 Python 环境/禁外网启动测试必须通过；网络功能使用本地可控服务器。纯浏览器 CORS/重定向限制单独记录，不能把 CORS 失败当作策略阻断成功。
- 外部进程测试记录回收状态；测试结束没有残留子进程。失败上传 trace、stdout/stderr、manifest 和资源信息。

### D. package / release

- 对验证过的原文件打包，不重新编译或优化；校验解包后的 SHA-256 并从干净目录再次做最小加载。
- main/手动构建上传可下载 artifact；tag 流程发布独立 standalone GitHub Release，长期消费不依赖临时 artifact 保留期。
- 第一期不要求 npm/crates.io/PyPI/GHCR 发布；移除对上游包名、组织镜像和 trusted publishing 的依赖。
- 发布说明列出已完成需求、接口版本、宿主要求与未支持能力；M1-M3 未完成时不能称为 AI 沙箱改造完整版。

## 5. 旧工作流整理清单

实施时逐项处理 `.github/workflows/`，每个文件明确“默认保留、手动 legacy、停用或迁移”。除了 wasm-build/release/ci，还需检查 pyodide、snapshot-runner、runner-dispatcher、remote-sandbox-e2e、images-dev、langchain-adapters 及其发布/LLM 工作流、examples-test、sonar。

Python、容器、LangChain 与上游服务相关工作流应退出默认 push/PR/tag 链路；如保留手动入口，说明其所需环境和非主交付地位。检查 branch protection 的 required checks 名称需要仓库设置配合，此项应在实际迁移时检查，不能仅靠改 YAML 假定已完成。

保留上游目录不等于继续默认构建它们；无需大规模移动核心 crate 或一次删除所有 Python 文档。优先实现可独立交付，然后按引用关系清理失效说明。

## 6. 从成功构建获取产物

实施成功后，仓库 Actions 对应 run 的 Artifacts 区提供下载，也可使用以下 CLI 流程（RUN_ID 与名称使用实际构建结果）：

```sh
gh run list --repo NotFaceGUI/wasmsh
gh run download RUN_ID --repo NotFaceGUI/wasmsh --name wasmsh-standalone
```

下载者需有仓库读权限；artifact 有保留期限，正式版本通过 Release 交付。[GitHub 官方 artifact 下载说明](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/download-workflow-artifacts)。

最终完成证据必须包括成功的 GitHub run 链接、提交 ID、产物名称及摘要、Node/browser/宿主矩阵测试结果。仅有 YAML 文件或 Rust 编译成功不满足核心交付目标。
