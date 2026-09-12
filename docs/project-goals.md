# AI Shell 沙箱：项目目标

状态：需求已实施并完成云端验证（截至 `v0.9.5`，2026-09-12）。分析日期：2026-09-09。

分析基线：`NotFaceGUI/wasmsh`，提交 `61811885b4f1f730fd6236b5d08be3c29615558b`。本文最初基于本地源码静态核对；后续 M0-M4 已实现，并由 GitHub-hosted runner 实际构建、三平台消费验证和发布，逐项证据见 [目标验证报告](verification-report.md)。

## 1. 核心目标

为 AI 助手提供一个可嵌入、行为稳定、权限由宿主控制的 Bash 兼容执行环境。AI 发出的脚本在相同版本、虚拟文件系统、配置和输入下具有一致的 shell 语义，不因宿主是 Windows、Linux 或 macOS 而改写为 PowerShell / 系统 sh。

**核心交付：由本仓库 GitHub Actions 编译、验证并提供下载的独立 sh WASM 与必要的 JS/类型声明/宿主适配代码，不携带 WASM Python、Pyodide、CPython 标准库或 micropip。**

“环境稳定”指解释器、工具、路径、编码和接口契约稳定；真实时间与已授权网络请求仍应反映当前外部世界。固定时间只能作为显式测试模式，不能成为生产会话的隐含行为。

## 2. 使用边界

| 范围 | 本项目决定 |
| --- | --- |
| 面向谁 | AI 助手的 Bash 工具调用；宿主应用负责会话、授权和工具接入 |
| 替换什么 | AI 执行入口对 pwsh、系统 sh 及系统基础命令差异的依赖 |
| Shell 兼容性 | 以 [SUPPORTED.md](../SUPPORTED.md) 和项目回归用例定义支持集合；不宣称完整 GNU Bash |
| 文件系统 | 默认会话隔离 VFS；POSIX 虚拟路径，宿主文件仅通过显式能力接入 |
| 基础工具 | 保留内置 shell、文本/文件/JSON 工具、受控 curl/wget |
| 时间 | 生产默认实时宿主回调；测试可固定或注入可控时钟 |
| 网络 | 默认禁用；支持白名单、黑名单、黑白名单组合及明确的域名通配规则 |
| 外部程序 | 宿主显式注册；接通 argv、stdin、stdout、stderr、退出码、超时与取消 |
| 主构建目标 | `wasm32-unknown-unknown`，从现有 `wasmsh-browser` 独立构建链路演进 |
| 排除内容 | WASM Python、pip/REPL、Pyodide 构建、Python SDK、Kubernetes/dispatcher 主部署流程 |
| 非目标 | 替换操作系统登录 shell、完整终端模拟器、PTY/作业控制、任意宿主程序执行 |

具体消费宿主尚未提供。本计划优先按桌面/服务端 JS 宿主的独立 WASM 接入设计，同时保留浏览器基础 shell 验证。若实际消费方使用 Go/Rust WASM 引擎，应先定义宿主 ABI；`wasm-bindgen` 产物不是可直接放进任意 WASI 引擎的通用可执行文件。

## 3. 已核对的现状

| 用户问题 | 当前源码事实 | 改造重点 |
| --- | --- | --- |
| 时间停留在启动时 | `date` 每次读 `WASMSH_DATE`，未设置时固定为 2026-01-01；没有实时 provider | 引入可注入时钟，接入实际 WASM 宿主，覆盖同一脚本中的多次读取 |
| 只有白名单且通配不好用 | 当前 `HostAllowlist` 已支持 `*.example.com`，但没有黑名单和显式默认策略 | 保留已有通配语义，补黑白名单组合、校验与宿主全链路执行 |
| external 无法用管道 | Rust handler 已接收 stdin，已有有限输入/重定向测试；JS `WasmShell` 未导出注册方法 | 先定位实际接入层；补宿主桥接与完整流式进程协议，不能只再加一个 stdin 参数 |
| 希望 Actions 编译 WASM | 已有 standalone 构建，但工作流使用 `mayflower-k8s-runners`，同时构建 Pyodide | 改成 fork 可用的独立构建、实物加载验证和独立产物发布流程 |

用户以前所用版本、external 注册封装和失败脚本没有出现在当前输入中，因此暂不能将历史问题归因为某一个已证实的宿主实现缺陷。上述两项已有支持也不表示用户遇到的问题已经解决。

详细源码位置、设计约束和验收案例见 [需求设计](design/ai-shell-requirements.md)。

## 4. 环境一致性契约

1. Shell 源码和文本默认 UTF-8；虚拟文件和管道保持原始字节，不隐式转换 CRLF、NUL 或二进制内容。
2. VFS 使用 `/` 路径和固定的虚拟 HOME/工作目录；宿主路径由适配器显式映射，不能把 `C:\\...` 当作虚拟路径直接传递。
3. 不继承宿主全部环境变量、PATH 或启动配置；只注入明示允许的变量。时区默认 UTC，避免宿主区域设置改变基础输出。
4. 同一会话按顺序执行，保留 cwd、变量、文件；不同会话相互隔离。重置会话与更新能力配置必须区分，不能为了更新时间反复 Init 并清空文件。
5. 原生 external 的程序版本、编码、参数和文件权限仍可能存在平台差异；这些由宿主适配契约和跨平台测试约束，WASM 不会自动消除它们。
6. 未支持的功能应返回非零状态和明确诊断，不应静默成功。提供可查询的版本、命令和能力信息供 AI 工具适配器使用。

## 5. 交付顺序

| 阶段 | 内容 | 退出条件 |
| --- | --- | --- |
| M0 构建基线 | 改造独立 Actions；固定工具链；编译 web/nodejs/bundler；从产物加载 shell | 本 fork 上产生可下载且实际加载通过的无 Python 产物 |
| M1 实时时间 | 公共时钟能力、WASM 回调、固定测试模式；接入 date 和 SigV4 | 同会话、同脚本多次取时正确；失败不返回伪造时间 |
| M2 网络策略 | 白名单/黑名单/组合模式、通配语义、规范化、逐跳校验 | Rust 和实际宿主对同一策略一致；被拒目标无网络访问 |
| M3 外部管道 | 独立宿主注册；先闭合有限 I/O，再接通流式进程、背压与取消 | 三段混合管道、重定向、二进制、大输出和中止全部满足契约 |
| M4 产品验收 | 同一 WASM 在三种桌面系统接入；完善 AI 命令能力与交付说明 | 一次干净构建的产物通过验收矩阵，可独立下载使用 |

M0 产物是基础版本，不得提前标记 M1-M3 已完成。有限缓冲 external 是过渡能力，只有流式验收通过后才可声称满足本项目 external 管道目标。

## 6. 项目整理原则

- 复用现有 Rust 核心、VFS、I/O 调度和测试组织方式，避免另写一套 shell。
- 先将 Python 相关构建、依赖、运行时资产与默认发布链路解耦；源码目录清理放在依赖确认之后，不以批量删目录代替解耦。
- 上游参考资料可以保留，但主 README、使用示例和产物说明必须清楚标出本 fork 的实际能力。
- 默认 CI 不再触发 Pyodide、LLM 付费测试、上游私有 runner 或上游包发布；逐个工作流梳理触发器，不能只新建一个工作流后留下旧流程并行排队。
- 保留 Apache-2.0 授权与上游来源信息；本 fork 的构建状态和产物链接使用本仓库，不把上游徽章当作本项目验证结果。

## 7. 总体验收

- [x] 本仓库 GitHub Actions 无须上游组织 runner、发布密钥或 Python 构建即可编译独立 WASM。（`Standalone Release` `34689670098`、`Standalone WASM` `34690078437` 全绿）
- [x] 产物包含 `.wasm`、配套加载器、类型声明、许可证、版本与 SHA-256 清单，干净目录可加载。（31 项 `SHA256SUMS` 全 OK，三 target 一致，Node/bundler smoke 通过）
- [x] Python/Pyodide 资产既不被打包，也不在启动时隐式下载；默认无 `python`/`python3` external 注册。（`verify-package.mjs` 断言，Node smoke 覆盖禁用 `fetch`）
- [x] 日期实时回调、网络策略、external 管道通过 [需求验收矩阵](design/ai-shell-requirements.md#6-验收矩阵) 的 Rust 与 Node/宿主部分；浏览器网络路径按设计失败封闭，浏览器无原生进程能力。（见 [验证报告](verification-report.md#5-尚未达成或有意的缺口)）
- [x] Windows/Linux/macOS 使用同一构建产物验证基础 shell 与宿主 external 契约。（release/`main` 工作流的三平台 host matrix 通过）
- [x] 超时、取消、输出限额、会话隔离及能力拒绝可观察且不会伪报成功。（`node-smoke.mjs` 断言 124/125/126/127、取消 130、会话隔离）
- [x] 记录每项测试使用的 commit、产物摘要、宿主环境与失败日志；明确未支持的 Bash/终端功能。（[实施跟踪](implementation-tracker.md)、[验证报告](verification-report.md)、[SUPPORTED.md](../SUPPORTED.md) 已知差异清单）

GitHub Actions 具体改造和下载交付要求见 [构建计划](guides/standalone-wasm-build-plan.md)。
