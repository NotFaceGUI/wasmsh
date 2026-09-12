# AI Shell 实施跟踪

用户要求：本阶段由主任务直接实现，不创建 worktree、不切换分支、不委派任务、不推送或发布。需求基线见 [项目目标](project-goals.md)、[需求设计](design/ai-shell-requirements.md)、[构建计划](guides/standalone-wasm-build-plan.md)。

执行方式：各阶段直接在 `E:\Work\wasmsh` 的当前分支 `main` 修改，不创建 worktree、不切换分支。每阶段记录实际改动和验证证据；云端未运行或平台未验证时明确保留未完成状态。

| 顺序 | 任务 | 状态 | 验收重点 |
| --- | --- | --- | --- |
| 1 | 独立 WASM 构建与 CI 解耦 | 代码完成，云端待验证 | GitHub-hosted runner、固定工具版本、无 Pyodide、实际产物测试与打包 |
| 2 | 实时时钟 callback | 代码完成，本地 WASM/单测通过，浏览器断言待环境 | live/fixed、同 Run 多次取时、date/SigV4、异常处理 |
| 3 | 网络黑白名单与通配 | 代码完成，本地核心验证通过，浏览器/云端待验证 | deny 优先、规范化、旧接口兼容、宿主逐跳阻断 |
| 4 | external 注册与有限 I/O | 代码完成，本地 runtime/WASM/Node 通过，浏览器待环境 | standalone JS 注册/注销/查询、原生 argv/stdin/stdout/stderr、FD 与退出码 |
| 5 | external 流式管道与取消 | 代码完成，本地 runtime/WASM/Node 通过，浏览器待环境 | 背压、提前结束、超时取消、无限输入与进程回收 |
| 6 | 集成测试与交付文档 | 代码完成，本地实际 WASM 通过，正式包/云端待验证 | Node/browser/bundler、clock/network/external、隔离/二进制/权限/取消、版本与 SHA256 硬门禁 |
| 7 | 主任务最终审阅 | 代码修复完成，本地回归通过，云端/浏览器待验证 | 回归、权限边界、错误状态、验证缺口与修复 |
| 8 | Bash 语义差分硬化 | 代码完成，本地 oracle 真实比对通过 | 建差分 oracle、修复逐项偏差、契约用例、CI oracle job、E2E ETL 验收 |

## 执行记录

- 初始源码提交：`61811885b4f1f730fd6236b5d08be3c29615558b`。
- 需求文档基线已保存为本地提交 `cc25ec8`。首次任务误用了 worktree，收到用户纠正后改为当前分支直接执行。
- M0 实现（2026-09-09）：新增 `tools/standalone/` 构建、打包、清单、SHA256、无 Python/Pyodide 资产检查和 Node/bundler smoke；`wasm-build.yml`、`ci.yml`、`release.yml` 迁移到 GitHub-hosted `ubuntu-24.04`，只在最终测试通过后上传/发布独立 sh WASM。默认 Rust toolchain 移除 Emscripten、Rust 脚本移除宿主特定 PATH，standalone Playwright 依赖改用 `npm ci` 锁文件。Binaryen 117 及归档 SHA256 已显式固定并在构建前校验；`wasm-pack --mode no-install` 不再允许静默跳过 `wasm-opt`。
- M0 本地证据：`cargo test --workspace --locked` 通过；`wasmsh-browser` 250 个单测和网络安全 13 个测试通过；`cargo build --target wasm32-unknown-unknown --release -p wasmsh-browser --locked` 通过；三种 wasm-pack 产物、解包后的 SHA256、Node smoke 和 bundler smoke 通过（这些产物是在本次加入 Binaryen 硬门禁前构建的）。Windows `--all-features` 仍会触发既有 Emscripten libc 模块兼容错误，未将其伪报为主链路通过；GitHub Ubuntu 的 all-features CI 尚未运行。
- M0 复核：workflow YAML、Bash/Node 脚本语法、`git diff --check` 和 `npm ci --ignore-scripts` 通过；使用本机缓存的、版本为 117 的 Binaryen 直接构建三个 target 后，最终包的 manifest、许可证/版本文件、SHA256、Node smoke 和 bundler smoke 均通过。当前 Windows 环境不执行 Linux x86_64 安装器；之前的 Chromium 下载因网络吞吐停止，浏览器断言和 GitHub Actions 仍未运行，不能把它们伪报为通过。
- M0 浏览器/云端状态：本地 Playwright 16 项未进入断言，因环境没有 Chromium；固定浏览器下载在本机无响应后停止。GitHub Actions 和正式 Release 未运行，没有成功链接、云端产物摘要或跨平台宿主证据，不能标记 M0 云端验收完成。
- M0 未完成项：本阶段没有实现网络策略扩展或 external 改造，也没有 `host/` 适配包；M0 云端验收仍待后续运行。
- R1 实现（2026-09-09）：新增公共 `wasmsh_utils::ClockProvider`，提供 `now_unix_ms()` 墙钟和 `monotonic_now_ms()` 单调读数；加入 UTC Unix 毫秒校验、固定 fake clock、legacy `WASMSH_DATE` 解析和 SigV4 共用采样。`WorkerRuntime` 保留会话 provider，`$SECONDS` 使用单调经过时间，`time` 使用单调 elapsed；WASM `WasmShell` 暴露 `set_clock_callback(Function)`、`clear_clock_callback()`、`set_fixed_time_ms(i64)`，JS worker 默认安装 `() => Date.now()`。回调异常、非安全整数、越界日期均返回非零诊断；`date -R`、`-I*`、`-d` 已实现，未知选项不再静默忽略。
- R1 本地证据：`cargo test -p wasmsh-utils -p wasmsh-runtime --locked` 通过（runtime 单元 54、协议 42、共享 I/O 10、utils 709）；`cargo check -p wasmsh-browser --target wasm32-unknown-unknown --locked` 与 `cargo build --target wasm32-unknown-unknown --release -p wasmsh-browser --locked` 通过。使用现有 wasm-bindgen web fixture 在 Node 中加载真实 `.wasm` 并调用 `set_clock_callback`/`exec`，固定时间、同 Run 序列采样、date 格式/`-R` 与 callback `NaN` 失败断言通过。新增 standalone Playwright clock 用例覆盖同 Run 多次读取、跨日、异常/非法数、date 选项和 SigV4 `x-amz-date`，但本机未安装 Chromium，4 项均在浏览器启动前失败，尚未进入断言；GitHub Actions 仍未运行。
- R1 接口供下一阶段沿用：Rust 侧使用 `wasmsh_utils::{ClockProvider, ClockError, FixedClock}`，runtime 通过 `WorkerRuntime::set_clock_provider(Box<dyn ClockProvider>)` 注入；standalone JS 侧使用 `shell.set_clock_callback(() => Date.now())`，测试可传固定/序列回调。provider 生命周期由 `WorkerRuntime`/`WasmShell` 持有，替换后旧 JS 函数释放；callback 必须同步返回安全整数 Unix 毫秒，跨 worker 不能通过 JSON 传函数。`WASMSH_DATE` 只在无 provider 的低层 legacy `UtilContext` 使用，生产 live provider 已安装时不会覆盖 date 或签名时间。
- R1 未完成项：standalone 构建脚本在本机工具链检查处阻断（PowerShell 可见 `wasm-pack`/`wasm-bindgen`，缺少 `wasm-opt`；bash 调用也未能发现 `wasm-pack`）；浏览器实际断言、GitHub Actions 产物和跨平台宿主尚未验证。`cargo fmt --all -- --check` 仍受仓库既有 CRLF/换行风格影响，未对全仓执行重格式化。`timeout` utility 仍是既有的概念性透传，真正的 wall-clock deadline/取消属于后续 external/runtime 能力，不把 step budget 冒充成超时实现。VFS 文件 mtime 仍按现有固定语义，不在本阶段改成墙钟时间。
- R2 实现（2026-09-09）：以 `wasmsh_utils::NetworkPolicy` 统一禁用、白名单、黑名单、黑白名单组合和显式全允许；默认 `enabled: false`，deny 规则先于 allow 规则。规则/URL 共用大小写、尾点、IDNA、有效 HTTP/HTTPS 默认端口、IPv4/IPv6、标签边界校验；保留 `*.example.com` 的严格多层子域语义且不匹配根域，支持 `*`、精确主机和可选端口，不支持的规则在初始化时报错。`allowed_hosts` 继续作为旧式 allow-only 配置，显式同时传入新旧字段会报错（包括 JSON 中空旧数组）。
- R2 宿主执行：`curl`/`wget` 共享 `NetworkBackend` 和策略包装器；`NetworkBackend::check_url` 与每次 `fetch` 都在实际 I/O 前执行。Rust 网络工具、Pyodide FFI、Node runner broker 和 Python/JS fetch membrane 均强制手动重定向，逐跳重检目标，跨源移除 `Authorization`、`Cookie`、`Proxy-Authorization`、`Host`，跨源 SigV4 重定向拒绝，并落实超时、重定向次数和响应体上限。浏览器同步 XHR 无法保证逐跳阻断，因此 standalone/Pyodide browser 路径默认明确拒绝；只有接入满足逐跳检查的可信 broker 后才能启用。
- R2 本地证据：`cargo test -p wasmsh-utils net_types --locked`（18 passed）、`cargo test -p wasmsh-json-bridge --locked`（4 passed）、`cargo test -p wasmsh-runtime --locked`（107 passed）、`cargo test -p wasmsh-browser --test network_security --locked`（14 passed）和 `cargo test --workspace --locked` 均通过。受控 TCP 重定向用例 `denied_redirect_target_is_never_received_by_controlled_server` 实际确认拒绝目标命中数为 `0`；验证不是依赖 CORS 错误。Node `allowlist-unit + fetch-broker-unit` 为 21 passed，`fetch-membrane-unit` 为 13 passed（含跨源返回初始源后仍不恢复敏感头）；`git diff --check` 通过。
- R2 未完成项：本机未安装 Chromium，standalone/Pyodide browser 的实际浏览器断言（包括同步 XHR 拒绝）未进入运行；Node runner 全量测试为 54 passed、19 failed，失败均发生在生产 runner 启动前，原因是本地缺少既有的 `packages/npm/wasmsh-pyodide/assets/pyodide.asm.wasm`，不是把 CORS/启动错误当成网络安全证据。可信 browser broker 尚未提供，GitHub Actions、真实发布资产和跨平台宿主也未运行。
- R2 下一阶段接口：Rust 使用 `wasmsh_protocol::NetworkPolicyConfig`、`wasmsh_utils::NetworkPolicy`、`wasmsh_utils::net_types::{NetworkBackend, HttpRequest, HttpResponse}`；协议入口为 `HostCommand::Init.network_policy`，旧入口为 `allowed_hosts`。JS/Node 使用 `networkPolicy`（结构化配置）和兼容的 `allowedHosts`，Node broker 的同步客户端通过 `createBrokerClient().fetchSync` 接入；浏览器如启用网络，必须接入可逐跳检查的可信 broker，而不能恢复裸同步 XHR。
- R3 第一阶段实现（2026-09-10）：`wasmsh-runtime` 增加经过校验的 `ExternalCommandSpec` 注册表、注销/查询、spec-aware handler 和有限 stdin/stdout/stderr 上限；旧 `ExternalCommandHandler` 保持兼容并复用原有 `ExternalCommandStdin`、`ExecIo`、`PipeBuffer` 和 FD 路由。注册项固定 executable，传入已解析 argv（含 argv[0]），不再拼接 shell 命令；注册但无可用 native executor 返回 126，未注册保持 127。新增 `WasmShell.register_external`、`unregister_external`、`external_commands`、`set_external_executor`、`clear_external_executor`，browser worker 加入注册/注销/查询消息；无 executor 的 browser 明确返回 native process unsupported。
- R3 Node 宿主实现：`tools/standalone/node-external-host.mjs` 使用 `spawnSync(executable, argv.slice(1), { shell: false })`，只注入注册项 env、不继承 `process.env`，要求显式 host cwd，关闭 stdin EOF，二进制传输并同时收集 stdout/stderr；支持显式 `vfs_path_mappings`、固定 `argv_prefix`、超时和限额。默认输入/合并输出 16 MiB、超时 30 秒；注册上限 64 MiB/300 秒；124=超时、125=有限 I/O 限额、126=启动/宿主能力失败、127=未注册。host adapter 已打入 standalone 产物的 `host/`。
- R3 本地证据：`cargo test -p wasmsh-runtime --test runtime_protocol --test shared_io --locked` 通过（44 + 14），其中 `shared_io` 包含管道暂存期 stdin 限额验证；`cargo check -p wasmsh-browser --target wasm32-unknown-unknown` 与 release WASM 构建通过；直接用生成的 wasm-bindgen Node loader 加载实际 WASM 后，`node tools/standalone/node-smoke.mjs dist/r3-pkg` 通过，覆盖三段混合管道、重定向顺序、here-doc、二进制、非零退出、`PIPESTATUS/pipefail`、argv/cwd/env/注册查询及 124/125/126；`node tools/standalone/external-smoke.mjs` 通过原生测试程序的相同边界。浏览器 external 测试已加入，但本机仍无 Chromium，尚未进入断言；正式 wasm-pack/Binaryen 产物和 GitHub Actions 仍未运行。
- R3 接口供下一阶段沿用：WASM executor callback 同步接收 `(command_name, fixed_executable, argv, stdin_bytes, options_json)`，返回 `{status, stdout, stderr}`；`ExternalCommandOptions` 字段为 `cwd`（host 路径，必须由 Node executor 明确提供）、`env`（完整导出白名单）、`vfs_path_mappings`、`argv_prefix`、`max_input_bytes`、`max_output_bytes`、`timeout_ms`。这是有限兼容接口：stdin 在 callback 前收集，stdout/stderr 在子进程退出后返回，不提供早期输出、背压或取消；下一阶段使用 start/write_stdin/close_stdin/read_stdout/read_stderr/wait/cancel 流式接口，并继续沿用现有 FD/PIPESTATUS/pipefail 语义。
- R3 第一阶段边界：上一阶段只完成有限同步 external；browser 只能注册并查询，不能启动原生 executable；浏览器可信 broker、跨平台宿主矩阵、正式 standalone 产物和云端验证待后续。流式生命周期在本阶段继续实现，但最终 external 验收仍需跨平台证据。
- R3 流式实现（2026-09-10）：新增非阻塞 `ExternalProcess`/`ExternalProcessPoll`/`ExternalProcessWrite` 接口和 `PendingStreamingPipeline`，仅由 `StartRun`/`PollRun` 驱动；external stdout/stderr 按分块进入现有 `PipeBuffer`，暂时无数据保留为 pending，stderr 不经 `|` 时独立排空。stdin 写入可报告部分接受和背压，关闭 stdin 发送 EOF；下游关闭 PipeBuffer 读端后取消 external，并向上游传播 BrokenPipe，暂存文件在 stage 结束/取消时清理。旧内置 scheduler 保留 1-byte 反馈队列，progressive external pipeline 使用独立 64 KiB 有界队列，aggregate pipe limit 仍由 VM budget 约束。
- R3 Node 流式宿主：新增 `createNodeExternalStreamExecutor()`，使用非 shell `spawn`、每路有界事件队列和 pause/resume、同步 `poll` 操作、stdin `drain` 状态、stdout/stderr EOF、退出码和显式超时；取消在 Windows 使用 `taskkill /t /f`，POSIX 使用 detached process group。stream callback 不接 Promise；Promise 只能在 WASM 调用之间由宿主事件循环推进。
- R3 流式本地证据：实际 release WASM 经 `wasm-bindgen --target nodejs` 加载后，`node tools/standalone/node-smoke.mjs dist/r3-stream-pkg` 通过，覆盖 `hostproducer | head`、`yes | hostcat | head`、大于 pipe capacity 的 8192-byte 输入、`hostdual` 双路 64 KiB 输出、`hostdual |& hostcat`、超时、启动失败、取消后恢复；runtime fake process 覆盖“暂时无数据不等于 EOF”和 head 关闭传播。`cargo test --workspace --locked`、WASM check/release build、Node smoke 和脚本语法检查通过。
- R3 流式未完成项：browser worker 没有 native process executor，只能注册/查询并明确失败；浏览器实际断言仍受本机无 Chromium 阻断。Node Windows 进程树路径已由本地 smoke 走通，Linux/macOS process-group 和最终 CI/跨平台矩阵尚未运行；WASM 正式 wasm-pack/Binaryen 包、GitHub Actions 和发布资产仍待后续。
- R3 下一阶段接口：继续使用 `WasmShell.set_external_stream_executor(request => result)`、`start_run(input)`、`poll_run()`；request operation 为 `start`/`write_stdin`/`close_stdin`/`poll`/`cancel`，poll result 必须返回分块 stdout/stderr、各自 EOF、stdin_writable 和 status。下一阶段产品集成应围绕该接口补宿主矩阵和可观测资源回收，不把 `exec` 的有限 `{status, stdout, stderr}` 接口当作流式能力。
- M4 集成实现（2026-09-11）：新增 `tools/standalone/node-network-host.mjs`/`.d.ts`，把 Node 的异步 `http`/`https` transport 放入独立子进程以满足 WASM 同步 callback；每次只执行一个请求，禁用自动重定向，执行请求/响应上限和超时，保留 `error_reason`。`wasmsh-browser` 将 `response_too_large` 等 host 错误映射为结构化网络错误。Node broker 只在宿主显式安装并设置 `set_trusted_network_broker(true)` 后启用。
- M4 集成测试（2026-09-11）：`node-smoke.mjs` 现在从传入 artifact 的 `nodejs/` loader 加载实际 WASM，覆盖实时 clock 同 Run 双采样与 callback 失败、独立本地 HTTP 的允许相对重定向、跨端口重定向目标请求数为 `0`、禁用网络、响应过大、有限/流式 external、二进制 stdin/stdout/stderr、argv/cwd/env、VFS 隔离、超时、取消和恢复；网络 fixture 使用独立 Node 子进程，避免同步 callback 阻塞测试 server。`bundler-smoke.mjs` 覆盖 bundler 加载、clock、二进制 VFS、权限拒绝和隔离；`examples/standalone/node.mjs` 是可直接运行的接入示例。
- M4 包硬门禁（2026-09-11）：`package.mjs` 将 network host 代码及声明纳入 `host/`，manifest 记录每个 target 的 WASM 字节数、gzip 大小和 SHA256；`verify-package.mjs` 验证 WASM 魔数、loader/声明/package metadata、`VERSION`/manifest/package version、一致的三份 WASM 和完整逐文件 `SHA256SUMS`，拒绝 Python/Pyodide 路径和 JS 远程启动 URL。Node smoke 在导入前替换 `fetch` 为抛错函数，验证 standalone 启动不下载 runtime 资产。
- M4 CI/文档（2026-09-11）：`wasm-build.yml` 和 standalone release workflow 均增加 `ubuntu-24.04`、`windows-2025`、`macos-15` 同一候选归档的 Node/bundler/external/clock/network smoke；正式 artifact 上传/Release 需 browser job 和三平台 matrix 同时成功。新增 `docs/guides/standalone-embedding.md`、`examples/standalone/`，并更新 README、SUPPORTED、sandbox/build plan 的基础能力、Node 完整接入和 browser 限制说明。
- M4 本地证据（2026-09-11）：`cargo test --workspace --locked` 通过；`cargo clippy -p wasmsh-browser -p wasmsh-runtime -p wasmsh-utils --all-targets --locked -- -D warnings` 通过；release `wasmsh_browser.wasm` 重新构建并经 `wasm-bindgen` 生成 nodejs/bundler/web loader，WASM SHA256 为 `F9BD7FDEC281B7B0843105320AC389902E82A2852107EEBB69FEDF5A68039ED8`。该 r3 WASM 上 Node smoke 连续 3 次通过，bundler smoke、standalone Node example 通过；r3 web loader 在本机 Chromium `140.0.7339.16` 上 standalone Playwright 23 项通过；`npm ci --ignore-scripts`、Node 脚本语法检查和 `git diff --check` 通过。
- M4 最终重验（2026-09-11）：rustfmt 后重新执行 `cargo build --target wasm32-unknown-unknown --release -p wasmsh-browser --locked`，原始 release WASM SHA256 为 `9F08C2D7A4D912CEBE8993BFE1A77B61468AD442F39CCC339DD4136B341DDB82`；仅从该文件生成 nodejs/bundler/web loader，三份处理后 WASM 字节一致，SHA256 为 `68F3E0C13F69A7383EAAF09427559C3388034FA07E2ED3BAD81FE66170CF1A32`。Node smoke 连续 3 次、Node external smoke、standalone Node 示例均通过；bundler smoke 已改为锁定 `esbuild 0.28.2` 的真实 ESM 打包消费并通过；使用同一 web loader、缓存 Chromium `140.0.7339.16` 的实际 executable 运行 standalone Playwright，23/23 通过。默认 Playwright executable 因本机缓存 1187 与依赖期望 1208 不匹配而失败一次，设置 `WASMSH_PLAYWRIGHT_EXECUTABLE` 后重跑通过；这属于本地浏览器安装选择，不是测试降级。`npm ci --ignore-scripts`、`cargo test --workspace --locked`（全量通过）、目标 clippy 和 `git diff --check` 均在该最终源码状态通过。`cargo fmt --all -- --check` 在本机只因未修改文件的 CRLF 工作树换行失败（`git ls-files --eol` 显示 `i/lf w/crlf`），本轮已格式化所有本轮修改的 Rust 文件，未重写无关文件。
- M4 未验证项：本机无法建立 GitHub/静态 Rust 下载 TLS，Windows 没有 Binaryen 117，WSL apt 只有 108；本轮实际运行 `bash tools/standalone/build.sh dist/standalone-pkg` 仍在工具发现阶段因 WSL 看不到 Windows `wasm-pack` 退出，PowerShell 也没有 `wasm-opt`。因此正式 wasm-pack + Binaryen 117、`package.mjs` 生成并解包正式归档、manifest/SHA256 的正式包检查尚未运行。r3 是 release Cargo WASM + 直接 wasm-bindgen 的真实消费验证，不是正式优化归档证据。GitHub Actions/Release 未运行；macOS/Linux 宿主、云端浏览器和云端产物摘要未验证。Browser native external 与可信 browser network broker 仍未实现，浏览器只验证注册/拒绝和网络安全失败闭合。
- M5 下一阶段接口：沿用 `WasmShell` 的 `set_clock_callback`/`set_fixed_time_ms`/`clear_clock_callback`、`init(step_budget, networkConfigJson)`、`set_trusted_network_broker`、`register_external`/`set_external_executor`、`set_external_stream_executor`、`start_run`/`poll_run`/`cancel`；Node 使用归档 `host/node-network-host.mjs` 和 `host/node-external-host.mjs`。下一阶段只需在 GitHub-hosted 三平台和可信 browser broker 上补正式归档/平台证据，不应重新引入 Python 下载或把 `exec` 宣称为流式接口。
- M6 最终审阅（2026-09-11）：对 cc25ec8 以来全部未提交实现做了独立审计并修复确认缺陷。R1：`performance.now()` 是小数，原整数校验导致 WASM 上单调时钟恒失败、`$SECONDS`/`time` 永远为 0，现已按小数截断处理；修正 `civil_from_days` 负 `z` 的整除 off-by-one；新增早期年份回归测试。R2：浏览器 `init` 在校验规则前就安装 transport，非法规则会留下未包装的放行 backend（配 `set_trusted_network_broker(true)` 时构成真实越权），现改为先校验并共享 `policy_ready` 门闩失败封闭；`NetworkBackend::check_url` 默认从“仅校验 URL”改为 `HostDenied`；浏览器错误分类补 `host_denied`/`too_many_redirects`/`payload_too_large`；wget 不再重试策略拒绝；新增失败封闭回归测试和 Node smoke 断言（校验后请求计数保持 4）。R3：新增 runtime 侧 wall-clock deadline（取各 external `timeout_ms` 最小值，超时按 124 收尾并写诊断）；构造中途 abort 时回收已启动进程；`write_stdin_chunk` 复核 `max_input_bytes`；移除 `poll` 的 `.expect` 潜在 panic；POSIX 取消补 SIGKILL 升级。M0/CI：产物门禁改用默认特性 `cargo test/clippy --workspace`（`--all-features` 移到 advisory job，因其 Ubuntu 构建从未验证）；`verify-package.mjs` 断言 Binaryen 与 wasm-opt 版本；`package.mjs` 增加 tag↔version 校验；新增 `.gitattributes` 固定脚本 LF。AI 可用性：`timeout` 不再假装成功（状态 125 + 诊断），`init` 播种确定性 `HOME=/home/user`、`PWD=/`、`PATH=/usr/bin:/bin`，`command -v`/`type` 可发现内置 utilities；同步修正 README/SUPPORTED/utilities/sandbox 文档的过度声明与过时 `Init` 示例。
- M6 本地证据：`cargo test --workspace --locked` 全量通过（wasmsh-utils 717、runtime 46+14、browser 250+14 network_security、其余套件全绿）；`cargo test -p wasmsh-testkit --test suite_runner --locked` 通过；`cargo clippy -p wasmsh-browser -p wasmsh-runtime -p wasmsh-utils --all-targets --locked -- -D warnings` 与默认特性全仓 clippy 通过；release WASM 重建后 `node-smoke.mjs`（含新增失败封闭断言）、`external-smoke.mjs`、`examples/standalone/node.mjs` 通过；真实 WASM 上验证 `$SECONDS` 随时间递增、`time` elapsed 非零、`command -v curl/jq` 与 `type curl` 可发现、`timeout` 返回 125、`HOME/PWD/~` 正确。`git diff --check` 通过；`cargo fmt --all --check` 仍仅因未修改文件的 CRLF 工作树换行报错。
- M6 浏览器验证（2026-09-11）：用当前源码 release WASM 直接 `wasm-bindgen --target web` 生成 `e2e/standalone/fixture/pkg`（CI 的 browser job 用归档 `web/` 平铺到同一路径），使用本机缓存 Chromium 1187 实际 executable，29/29 Playwright 通过：6 个文件覆盖 worker smoke、clock（同 Run 双采样、回调异常/NaN、`-R`/`-I`/`-d`、SigV4 拒绝）、file-ops、cancel、external 注册/查询/127、network-security 失败闭合，以及本轮新增 `review-fixes.spec.ts`（5 项：确定性 HOME/PWD/PATH 与 `cd`/`~`、`timeout` 状态 125、`command -v`/`type` 发现 utilities、非法规则 init 失败封闭、新旧网络配置冲突报错）和 clock monotonic `$SECONDS` 递增断言。设置 `WASMSH_PLAYWRIGHT_EXECUTABLE` 指向 1187 是本地浏览器版本选择（依赖期望 1208），不是测试降级；`e2e/standalone/build.sh` 现在复用 `tools/standalone/build.sh`，本机缺 `wasm-opt` 时不能直接跑，浏览器本地验证以直接 wasm-bindgen 生成的 loader 完成。
- M6 未验证项：GitHub Actions/正式 Release 未运行，正式 wasm-pack + Binaryen 117 归档与三平台 matrix 未执行；`--all-features`（OPFS/Emscripten）Ubuntu 构建仍未验证，故仅列 advisory；可信 browser network broker 与 browser 原生 external 仍未实现；M4/M5 列出的云端与跨平台缺口继续有效。审阅同时记录：external 流式协议仍无 run/session id（进程定位依赖宿主 Map 与单调 id），`stream_queue_bytes` 仅传宿主未用于 runtime 侧 PipeBuffer 尺寸，`2>&1`/`|&` 的 stdout/stderr 逐字节交织不保证；这些不阻塞本阶段验收，但应在后续协议版本化时处理。
- 不自动发布注册表包、创建正式 Release、修改仓库外部权限或推送 main；先完成可审阅的代码和本地验证。构建工作流可在后续获准的远端分支上运行。

## 8. Bash 语义差分硬化（2026-09-12）

用户要求：把沙箱修到与真实 bash 语义一致，并由差分测试守护，不分阶段交付。

本轮提交：`97eebd1`（语义硬化）→ `3aeb107`（版本 0.9.0 + README）→ `ccd7837`/`f4e23ec`/`50092dc`（release 暴露问题的修复）。

- **版本与发布（2026-09-12）**：`tools/bump-version.sh 0.9.0` 同步全部 manifest（Cargo workspace + 内部 pin、`wasmsh-pyodide`/`-probe`、npm/py 包、Helm `appVersion`），并同步 `langchain-wasmsh` 两个包的版本。README 去掉 “fork” 措辞、SUPPORTED 章节标题改为 `Delivery Status`。推送 `main` 并创建 `v0.9.0` tag，触发 `Standalone Release` workflow。
- **release 暴露并修复的 3 个问题**：
  1. `oracle.rs` 的 Git-for-Windows 回退用运行时 `cfg!(windows)` 守卫却调用 `#[cfg(windows)]` 函数，导致 **Linux 编译失败**；改为 cfg 守卫语句，并用 `--target x86_64-unknown-linux-gnu` 在本地交叉校验核心 crate 与 testkit 通过。
  2. `line_continuation_before_redirect` 用例写 `/in.txt`、`/out.txt`，在 Linux 上真实 bash 无权限写根目录而失败；改为相对路径（wasmsh 用 VFS cwd、oracle 用临时目录，互不干扰且可移植）。
  3. `bump-version.sh` 用 `cargo generate-lockfile` 会重新解析全部依赖，把 `wasm-bindgen` 从 0.2.127 浮动到 0.2.128，与 `versions.env` 固定的 `wasm-bindgen-cli 0.2.127` 不匹配，导致 `wasm-pack --mode no-install` 构建失败；改用 `cargo update --workspace` 只移动本地成员版本，并还原 lockfile。
- **发布结果**：`Standalone Release` run 34675198161 全绿——validate（workspace 测试 + suite + clippy）→ build candidate → 三平台 host matrix（ubuntu/macos/windows）Node/bundler/external smoke → Verify release candidate → Upload tested artifact → **Publish standalone GitHub Release**（Release v0.9.0 已创建）。此前两次失败分别由上述第 1、2、3 点造成，均已修复后才重新打 tag。
- **读回验证限制**：发布完成后本机 GitHub API/`gh` token 失效、curl TLS 抖动，无法再从本机读回 Release 元数据；发布成功的依据是 workflow 中 `Publish standalone GitHub Release` 任务的成功结论与 `git push` 的输出。

- **C 类（用户未确认项）结论**：两项**都是 bug**。C20 `awk 'BEGIN{s="abc   "; sub(/ +$/,"",s)}'` 只删一个字符——根因是 `posix-regex` 对以 `$` 结尾的模式不做 POSIX 左最长匹配，已在 `regex_posix::find/leftmost_match` 用锚定回扫修正；C21 `awk 'BEGIN{printf "%c",65}'` 输出 `6`——`format_char_spec` 把数字当字符串取首字符，已改为把数字当字符码。另在 oracle 下**新发现**两条静默偏差并修复：`awk -F'\t'` 不解析转义（`decode_awk_escapes`）、`$(( ... $(cmd) ... ))` 忽略命令替换（运行期先解析）；以及 token 间 `\`+换行未作续行（`skip_blanks`）。

- **差分 oracle**：`crates/wasmsh-testkit/src/oracle.rs` 重写为默认启用（`WASMSH_ORACLE=0` 关闭），通过 `WASMSH_ORACLE_BASH` 或 `PATH`/Git-for-Windows 常见位置发现真实 `bash`；脚本在私有临时目录以 `bash -c` 运行；Windows 上设置 `MSYS=winsymlinks:lnk` 以便真实创建符号链接。找不到 shell 的 oracle 用例返回可见 SKIP（`runner.rs`），绝不静默通过；stderr 差异在 `ignore_stderr=false` 时也参与比较。
- **新增契约用例**：`tests/suite/differential/` 22 个 `oracle.compare = true` 用例，覆盖本轮每条修复；全量套件 627 个 TOML 用例（621 通过、6 个 feature-gate SKIP、0 失败）。`WASMSH_SUITE_FILTER` 可只跑子集。
- **修复的偏差**（每条先红后绿，并与真实 bash 对照）：`sort -k N -n`/`-k2n`/`-t: -k2 -n`/默认稳定性/`-r`；awk `print`/`printf` 的 `>`/`>>`/`| "cmd"` 重定向、数组按引用传参、`$` 锚点左最长匹配、`printf "%c",N` 数字码、`-F'\t'` 转义；`while read` 管道/heredoc/重定向只迭代一次；`${VAR:-数字}`；无 else 的 `if` 返回 0 且不触发 `set -e`；`$(cmd)` 赋值状态；`{ }`/`( )` 分组重定向；引号 heredoc 分隔符；`\`+换行续行（词内、双引号内、token 间）；`trap ... EXIT` 脚本结束触发；`ln -s`/`cp -s`/`ln -sf`/`readlink` 真实符号链接；`grep -A/-B/-C` 上下文（含 `-A2` 粘连）；`wc -l` 无末尾换行、GNU 列对齐（单文件/多文件/管道/`<` 重定向）；`getopts` 的 `OPTARG`/`OPTIND`/聚簇/`-bval`/`--`/`:` 静默模式；`tar -C`；`sh file`/`sh -c` 子 shell 语义；算术展开内的 `$(...)`。
- **回归与非空洞守卫**：`crates/wasmsh-fs` 新增 6 个符号链接单测；`wasmsh-parse` 新增 heredoc/续行单测；`wasmsh-utils` 新增 CRLF/BOM/NUL/超长行/Unicode/空文件等边界单测。
- **CI**：`.github/workflows/ci.yml` 新增 `oracle` job（ubuntu 有 bash），先断言 `command -v bash`，再以 `WASMSH_ORACLE=1` 跑差分套件；`ci-pass` 已把 `oracle` 纳入必需依赖。
- **端到端验收**：`e2e/etl/{gen,run,verify}.sh` 多文件离线 ETL（nginx 日志 + 脏 CSV + JSONL → 规范化留痕 → 不依赖 `join` 的 awk 对账三表 → region 聚合 + 日志统计 → jq summary.json → Markdown 日报 → tar + sha256sum MANIFEST）。`crates/wasmsh-testkit/tests/etl_acceptance.rs` 在单一沙箱会话内跑两次：`set -euo pipefail` 生效、`PIPESTATUS` 正确、`verify.sh` 用另一种算法重算不变量并通过、两次运行 MANIFEST 内容哈希逐字节一致（幂等），并且故意破坏 `matched.tsv` 后 `verify.sh` 必须失败；denominator 为 0 时明确判失败。
- **本地证据（2026-09-12）**：`cargo test --workspace --locked` 1565 通过、0 失败；`cargo test -p wasmsh-testkit --test suite_runner --locked` 621/627 通过（6 个 pre-existing feature-gate SKIP）；`cargo clippy --workspace --all-targets --locked -- -D warnings` 干净；`WASMSH_ORACLE=1` 下 22 个差分用例全部与真实 bash 逐字节一致。
- **仍未支持/已知偏差**（诚实清单，见 `SUPPORTED.md`）：`sleep` 立即返回、`nproc` 固定值、`timeout` 对进程内命令不真正计时、`ulimit` 只读、`stat`/`date` 为子集；locale 固定 UTF-8/C；`ls -l` 的 owner/group/时间固定；工作树 CRLF 脚本按字面处理。`timeout`、`sleep` 桩、`nproc` 桩均已在文档标注，不再宣称完整 bash 兼容。

## 9. 沙箱可用性与静默偏差硬修（2026-09-12 续）

触发：一次以 wasmsh standalone WASM 为唯一 bash 沙箱的插件实测，在深层嵌套输入下踩到 wasmsh 崩溃，且把 `WasmShell` 实例永久毒化（`recursive use of an object detected...`），随后差分审计又暴露一批"返回 0 但结果错"的静默偏差。本轮在 `main` 上直接修复，全部由差分用例或单测守护。

- **致命崩溃根因与修复**：递归下降的 shell parser / 算术求值器 / awk 表达式 parser 均无递归上限，深层嵌套按字节数溢出调用栈；栈溢出是硬 abort，WASM 下不可捕获的 trap 会跳过 wasm-bindgen 的借用释放，导致实例永久不可用。四处均加深度上限并以普通错误返回：`MAX_NESTING_DEPTH=24`（parse）、`MAX_LEX_DEPTH=48`（lexer 的 `$( )` 扫描，嵌套输入在执行期会逐层重新 lex，故在下限处就截断）、`MAX_ARITH_DEPTH=64`、`MAX_AWK_DEPTH=64`；运行期 `MAX_RECURSION_DEPTH` 由 100 降到 48（原来 ~84 层命令替换即溢出，48 给 ~2x 余量，仍支持深度 ≤40 的递归函数与阶乘等真实用法）。实测溢出点分别约 59/84/~1000/~200/~120，上限取 ~1 MiB 原生/WASM 栈的安全值。
- **递归可恢复**：函数递归耗尽改为命令级（不再终止整个 run），`f(){ f; }; f || true; echo after` 现在输出 `after`；结构化 `StopReason::Exhausted(RecursionDepth)` 仍保留可观测。
- **`return` 之前完全不生效**：内建 `return` 只返回状态码，运行期没有解绑机制，`return` 之后的语句照常执行、递归函数不会终止。新增 `ExecState::return_requested` + `RuntimeCommandKind::Return`，函数帧与循环共同遵守，`return` 现在从函数及嵌套循环中正确退出。
- **静默错结果修复**：`$((x/0))`/`$((x%0))` 返回 0 → 现在失败并写诊断（`_ARITH_ERROR` 通道）；算术中的 `$` 参数被丢弃（`$(( $1 + 1 ))` 用字面量 1、`$(( $# ))` 为 0）→ `$1`/`$#`/`$?`/`${x}`/`${a[i]}` 现在正确解析；`stat -c %a/%A/%f` 硬编码 644/755 与 `ls -l` 矛盾 → 改为渲染 VFS `mode`；`echo a#b` 被截断 → `#` 仅在词首作注释；`${#@}`/`${#*}` 算成拼接后的字符数 → 改为位置参数个数；`${arr[@]:o:l}` 展开为空 → 实现切片；`${x:?msg}` 打到 stdout 且退出 0 → 改为 stderr + 脚本失败（`_SHELL_ERROR` fatal 通道）；只读变量赋值被静默丢弃 → 现在失败并诊断。
- **字段拆分修正**：`for` 词表按引号语义拆分（`for w in "a b"`/`a\ b` 是单字段，`$x` 按 IFS 拆，`"${a[@]}"`/`"$@"` 每元素一个字段），且不再对已解析的命令替换做二次错误拆分。`split_for_word` 在 AST 层区分字面/带引号/替换。
- **词法与转义**：新增反引号 `` `...` `` 命令替换（词法与词解析两层，双引号内也生效）；`$'\101'`/`$'\x41'` 与 printf 格式串的 `\NNN`/`\xNN` 解码；`%b` 的 `\0NNN` 与 `\NNN` 两种八进制形式。

- **新增用例**：`tests/suite/differential/` 新增 `hash_inside_word`、`arith_param_special`、`func_return_unwinds`、`for_word_split_quoting`、`arg_count_and_slice`、`nesting_depth_limit`、`arith_depth_limit`、`param_error_fatal`、`stat_mode_reflects_chmod`、`backtick_substitution`、`ansi_c_and_printf_escapes`；`a12_div_by_zero`、`cx30_readonly_enforcement` 更新为正确语义（并对真实 bash 做 oracle 比较）。Rust 单测：parser 深层嵌套被拒、算术深度保护、除零记录错误、算术 `$` 参数、awk 深度保护、ANSI-C 八进制。
- **feature 登记**：`features.rs` 补 `positional-parameters`、`word-splitting`；修正 `sandbox/recursion_limit_recovery` 里写错的 `or-list` → `and-or-list`。
- **文档**：`SUPPORTED.md` 新增 "Fixed in the sandbox-hardening pass" 与 "Remaining known divergences"，明确 `timeout`/`sleep`/`nproc` 桩、缺 `join`/`od`、`jq/yq --version`、数组负长度切片等仍存差异。
- **本地证据（本轮）**：`cargo test --workspace --locked` 全绿；TOML 套件 631 通过、5 个 feature-gate SKIP、0 失败（含 54 个 `differential/` 用例全部与真实 bash 逐字节一致）；`cargo clippy --workspace --all-targets --locked` 干净；`cargo fmt --all` 已跑。独立 `sh-audit` 差分 harness：修复前 16 MATCH / 2 CRASH，修复后 29 MATCH / 0 CRASH，深层嵌套输入全部变为优雅错误且会话可继续使用。
- **仍未做**：`join`/`od` 未实现；`jq`/`yq --version` 仍被当过滤器；`timeout`/`sleep`/`nproc` 仍为桩；数组负长度切片 `${a[@]:1:-1}` 与 bash 的报错行为不同；`readonly` 赋值为命令级失败而非脚本级 abort（bash 自身在 `;` 与换行下不一致）。以上均在 `SUPPORTED.md` 记录。

## 10. v0.9.1 发布（2026-09-12）

- **版本**：`tools/bump-version.sh 0.9.1` 同步全部 manifest（Cargo workspace + 内部 pin、pyodide crates、npm/python 包、`langchain-wasmsh` 两个适配包、Helm `appVersion`）。锁文件仅移动 workspace 成员自身版本，`wasm-bindgen` 等第三方 pin 未变，与 `versions.env` 固定的 `wasm-bindgen-cli` 保持一致。
- **提交**：`4bad0b4` `fix(sandbox): bound parser recursion and close silent-semantics divergences` + `5730aa5` `chore(release): bump version to 0.9.1`，推送 `main` 后打 tag `v0.9.1`。
- **发布结果**：`Standalone Release` run `34681588108` 全绿——Validate release source（2m11s，在 ubuntu-24.04 上跑 workspace 测试 + suite_runner + clippy -D warnings）→ Build release candidate（5m32s）→ Release host consumption ubuntu-24.04 / macos-15 / windows-2025 三平台全过 → Verify release candidate → Upload tested release artifact → **Publish standalone GitHub Release**。Release v0.9.1 已创建，asset：`wasmsh-standalone-0.9.1-5730aa520b4f.tar.gz`（3,860,855 bytes）。
- **真实 WASM 产物复核**（此前只在原生内核验证，本轮补齐）：下载 Release 资产解包，用 `nodejs/` loader 在同一 `WasmShell` 会话内实测——四种深层嵌套（算术 2000 层、awk 400 层、命令替换 200 层、`if` 200 层）全部返回普通错误，且随后的 `echo SESSION_ALIVE` 正常执行，证明实例不再被 trap 永久毒化；`return` 正确解绑（`a`/`done`）、`$((1/0))` 返回 st=1 并写 stderr、`echo a#b` → `a#b`、`$(( $1 + 1 ))` → 8、`"${a[@]}"` → `[x][y][z]`、反引号 → `[bt]`、`printf 'A\101B\n'` → `AAB`，均在真实 WASM 上确认。
- **发布产物校验**：`build-manifest.json` version=0.9.1、ref=v0.9.1、commit=5730aa520b4f。


## 11. v0.9.2 —— 沙箱差分审计驱动的语义修复（2026-09-12 续）

触发：一次以 wasmsh 为唯一 bash 沙箱的多阶段审计交付中，harness 报告了一批"进程级致命"与"静默错结果"。本轮先**直接复现**再动手：把所有"致命"触发逐条作为脚本跑真实内核并与真实 bash 对拍，结论与报告相反——14 条"进程级致命"里没有一条会终止运行时（详见审计交付 `DIVERGENCES.md` 第 1 节与 `cases-fatal/VERIFICATION.txt`）。真正需要修的是若干**静默错结果**与**作用域**缺陷。

- **更正的前提**：此前认为 command-not-found、`set -e`、`set -o pipefail`、`${PIPESTATUS[*]}`、`set -u`、`${x:?}`、`$((1/0))`、脚本以非零结尾、脚本内 `exit N`、函数体内 `grep` 无匹配、`trap EXIT`、递归、`set -o noclobber`、`>(...)` 会杀死整个进程。逐条复测均不成立（`$((1/0))` 与递归是"更宽容"而非致命）。审计侧据此取消了全部 fixture 排除，PD2 由 49/60 变为 60/60。
- **sed（`streaming_sed.rs`）**：`s/^/X/` 丢首字符（`^` 被 `posix-regex` 当作消耗一个字符）、行尾 `[[:space:]]*$` 不裁剪、`s/a*/Y/g` 零宽匹配多吞一个字符。改为把 `^`/`$` 拆出作为**行首/行尾约束**由调用方判定，零宽匹配后按 sed 规则抑制紧随的非空匹配。
- **路径展开（`expand` + `runtime` + `pattern`）**：引号是"整词"而非"逐字符"，导致 `"$dir"/*.sh` 完全不展开；未加引号的 `$p`（值内含 `*`）也不展开（VM 子集把裸 `Parameter` 当安全词）。改为逐字节引号掩码 + 掩码内转义元字符，并让含未引号参数/算术的词走完整解释器；glob 匹配器支持 `\*` 字面量。
- **参数切片（`expand`）**：`${v: -2}`、`${v:1:-2}` 返回空。负偏移/负长度改为从末尾计数，越界按 bash 返回空。
- **here-doc（`expand` + `runtime`）**：未加引号的 here-doc 不展开 `$(( ))`、`$( )` 与反引号；顺带修正 `scan_arith_double_paren` 把 `$((7))` 误判为 `$( (7) )` 的扫描错位。
- **子 shell 作用域（`runtime`）**：`sh -c`/`sh file` 继承调用者的 `set -u/-e/pipefail`（bash 会重置）；子 shell 内的致命展开（nounset、`${x:?}`、递归耗尽）或 `exit` 会终止**整个运行时**；`( … )` 内定义的函数/别名泄漏到父级；子 shell 的 `trap … EXIT` 从不触发。分别以"新进程重置 set 选项""子 shell 内捕获 exit_requested 并还原""子 shell 保存/恢复 functions+aliases""孤立子 shell 结束时补跑 EXIT trap"修复。
- **`read`（`wasmsh-builtins`）**：按 IFS 拆分后把字段用空格重新拼接，**破坏制表符分隔数据**（`read -r a rest` 得 `b c` 而非 `b	c`）；`-r` 被忽略。改为保留分隔符原文、实现 `-r` 与反斜杠转义语义。对 TSV/ETL 场景影响最直接。
- **`printf --`**：未把 `--` 当作选项终止符，`printf -- '%s
' x` 打印字面量 `--`。
- **`xargs -I{}`（`data_ops`）**：只支持分离形式 `-I {}`，GNU 的粘连形式 `-I{}` 报解析错。
- **`cmp`（`trivial_ops`）**：不支持 `-` 表示 stdin，二进制往返校验无法用管道完成。
- **新增差分用例（11 个，均先红后绿并与真实 bash 逐字节对拍）**：`sed_line_anchors`、`glob_quoting_mask`、`param_substring_negative`、`heredoc_expansions`、`child_shell_option_isolation`、`child_shell_exit_trap`、`subshell_scopes_fatal_and_functions`、`read_preserves_separators`、`printf_double_dash`、`xargs_attached_replace`、`cmp_stdin_dash`。
- **`tools/bump-version.sh`**：此前遗漏两个 langchain 包，v0.9.1 只能手工补；现已把 npm/python 的 `langchain-wasmsh` 纳入循环。
- **发布**：`tools/bump-version.sh 0.9.2` 同步全部 manifest（Cargo workspace + 内部 pin 17 处、pyodide crates、npm/python 四个包、Helm `appVersion`）；三个 Cargo.lock 仅移动 workspace 成员自身版本，第三方 pin 未浮动。
- **本地证据（本轮）**：`cargo test --workspace --locked` 全绿；`cargo test -p wasmsh-testkit --test suite_runner --locked` 全绿（含新增 11 个差分用例）；`cargo clippy --workspace --all-targets --locked -- -D warnings` 干净；`cargo fmt --all` 干净。
- **审计交付侧**：`E:\Work\work\sh-audit` 的 `build.sh` 重新自包含（26 个 heredoc 全部与宿主逐字节一致），修正 6 条错误的期望值、把硬编码的 "PROCESS-FATAL" 探针改为真实执行、修正分类器（表头误计 + 未写 `pg2.lang.status`），并按实测重写 `DIVERGENCES.md`/`README.md`。整条流水线现在**单次调用**跑完，`RUN-ALL.status=0`、PG2 60/60、`test_cases` 27/27、`verify` 18/18。
- **提交**：`5bbc7b8` `fix(semantics): close silent-result and scope leaks found by a bash differential audit` + `7f8328a` `chore(release): bump version to 0.9.2`，推送 `main` 后打 tag `v0.9.2`。
- **发布结果**：`Standalone Release` run `34685634969` 全绿——Validate release source（1m49s）→ Build release candidate（5m34s）→ Verify release candidate（46s）→ Release host consumption macos-15 / windows-2025 / ubuntu-24.04 → Upload tested release artifact → **Publish standalone GitHub Release**。Release v0.9.2 已创建：https://github.com/NotFaceGUI/wasmsh/releases/tag/v0.9.2 ，asset `wasmsh-standalone-0.9.2-7f8328ad4fde.tar.gz`（3,876,671 bytes）。
- **真实 WASM 产物复核（发布后读回）**：下载 Release 资产，`sha256sum -c SHA256SUMS` 全 OK，`build-manifest.json` version=0.9.2 / ref=v0.9.2 / commit=7f8328ad4fde。用 `nodejs/` loader 在同一 `WasmShell` 会话内跑 18 条断言，覆盖本轮每条修复——`sed 's/^/X/'`→`Xab`、`s/[[:space:]]*$//` 裁剪、`s/a*/Y/g` 零宽匹配、`"$d"/*.sh` 展开、`${v: -2}`/`${v:1:-2}`、here-doc 内 `$(( ))`/`$( )`、`read` 保留制表符与 remainder、子 shell 不继承 `set -u`、`( … )` 内致命只终止子 shell、子 shell 函数不外泄、子 `sh` 的 EXIT trap 触发、`printf --`、`xargs -I{}`、`cmp -`，**18/18 PASS**，且末尾 `echo SESSION_ALIVE` 正常，证明实例未被 poison。


## 12. v0.9.3 —— 输出进程替换 panic 与实例毒化修复（2026-09-12 续）

触发：下游报告 `tee >(wc -c > /tmp/_ps.txt) <<< hi` 让 standalone WASM 以 `panicked at lib.rs: unreachable: buffered pipeline stage requires runtime access` trap 退出，并且该 `WasmShell` 对象之后永久不可用（`recursive use of an object` / `while it was borrowed`），`v0.8.0`~`v0.9.2` 全部可复现。

- **复现**：原生内核不崩（`wasmsh-dev` 能拿到 isolated runtime），只有注册了 external handler 的配置才崩——而 `WasmShell::new()` 恒装 `external_spec_handler`。据此写出集成测试：`WorkerRuntime` + `set_external_handler(...)`，一行即稳定复现同一 `unreachable`。
- **根因**：管道阶段分两类，`BufferedCommand`（`cmd > file`、复合命令）与 `External`（宿主可执行）必须在**拥有 runtime** 的 runner 里 poll。`<(cmd)` 的构建器对此有守卫：需要 runtime 且拿不到 isolated runtime 就返回 `None`，落回缓冲捕获；`>(cmd)` 的构建器**漏了同一守卫**，照建 runner，命令结束走 `finish()` → `poll_without_runtime()` → 命中 `unreachable!()`。
- **修复一（消除 panic）**：抽出 `pipeline_requires_runtime()`，`try_build_live_process_subst_runner` 用与 `<(cmd)` 相同的守卫：`clone_for_isolated_process_subst()` 为 `None` 且管道需要 runtime 时返回 `None`，改走 `execute_inner_capture_stdout` 缓冲回退。命令正常完成，不再 panic。
- **修复二（消除静默丢数据）**：`tee >(consumer)`、`cp x >(consumer)` 这类把替换路径当普通 argv 打开的工具，数据写进 VFS 而不经 runtime sink，导致回退路径下 consumer 收不到任何内容；`flush_process_subst_out` 现在在该文件非空时读取并删除它，作为 payload 喂给 consumer（与 `cmd > >(consumer)` 的 sink 路径互补）。
- **修复三（fail-soft）**：`poll_without_runtime` / `close_without_runtime` 里那两个持有不变量的 `unreachable!()` 改为"关闭输出管道并报告 finished"，即使不变量将来被破坏也只是该命令无输出，而不是 abort 整个模块。
- **关于 catch_unwind**：`wasm32-unknown-unknown` 实测 `panic = "abort"`（`rustc --print cfg` 确认），panic 是不可捕获的 trap，`catch_unwind` 无法释放 wasm-bindgen 借用标记，因此**结构性消除可达 panic** 才是唯一持久修法；报告里"catch 后显式释放借用"的建议在本目标下不成立，已如实记录在 `SUPPORTED.md`。
- **回归**：新增 `crates/wasmsh-runtime/tests/output_process_substitution.rs` 9 条断言——3 个原 panic 触发、3 个原本正常的对照（`<(cmd)`、`> >(cat)`、`tee >(cat)`）、`tee >(consumer > file)` 与 `cmd > >(consumer > file)` 的落盘校验，以及"连跑三个触发后同一实例仍能 `echo alive`"的复用断言。
- **本地证据**：`cargo test --workspace` 全绿（50 个测试目标）；`cargo test -p wasmsh-browser --lib` 250 通过（该 crate 的 `run_shell` 正是无 isolated runtime 的配置，含 `process_subst_out_*` 4 条既有用例）；TOML 套件全绿；`cargo clippy -p wasmsh-runtime --all-targets -- -D warnings` 干净；`cargo fmt --all` 干净。


## 13. v0.9.4 —— 子 shell 内 EXIT trap 修复（2026-09-12 续）

触发：0.9.2 复跑报告把四条语言差异列为"真实"，其中 `c77-trap-exit` 是唯一真正仍具破坏性的行为差异——`trap ... EXIT` 在 `( … )` 内安装时不触发。逐条复核对拍后：

- **三条是报告侧误报**：`c07-empty-quotes`、`c32-case-fallthrough`、`c47-printf-c` 的"bash"列填错，实测 bash 与 wasmsh 一致——`bash -c "echo \"['']['x']\""` → `['']['x']`（单引号在双引号内是字面量）、`case a;;& b`（b 不匹配）→ 仅 `A`、`printf '%c' 65` → `6`（GNU coreutils printf 同为 `6`）。三条用例在 0.9.2 复跑中本就 PASS，报告表格与结果自相矛盾。
- **一条是真 bug**：`( trap 'echo INNER' EXIT; echo body ); echo after` 在 bash 下输出 `body / INNER / after`，wasmsh 只输出 `body / after`。根因：子 shell 执行体没有"结束时触发自身 EXIT trap"的步骤，而子 shell 语义上是一个独立进程，在 `( … )` 内注册的 trap 应在其结束时触发。
- **修复**：`HirCommand::Subshell` 分支现在保存继承来的 `_TRAP_EXIT`/`_TRAP_IGNORE_EXIT`，在子 shell 作用域内清空，执行 body 后以子 shell 状态调用 `run_exit_trap_if_needed(..., false)` 并把事件并回父级缓冲，再恢复父级 trap 变量。父 shell 安装的 trap 仍不在子 shell 内触发（与 bash 一致：只在外层 shell 退出时触发一次）。
- **回归**：新增 `tests/suite/differential/subshell_exit_trap.toml`，覆盖"子内安装触发""子内安装 + `exit 3`""父级安装不在子内触发"三种组合，与真实 bash 逐字节对拍。
- **本地证据**：`cargo test --workspace` 全绿（50 个测试目标）；TOML 套件全绿；`cargo fmt --all` 干净。

## 14. v0.9.3 发布（2026-09-12 续）

- **提交**：`d12dc09` `fix(runtime): stop >(cmd > file) from panicking and poisoning the WASM instance` + `e96d0c6` `chore(release): bump version to 0.9.3`，`panic = "abort"`（wasm32）下无法 catch，故结构性消除可达 panic。
- **发布结果**：`Standalone Release` run `34687056625`——Validate release source 通过（含新增 9 条输出进程替换回归）。
