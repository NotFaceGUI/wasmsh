# 独立 sh WASM 目标验证报告

验证日期：2026-09-12
验证对象：`NotFaceGUI/wasmsh`，`main` @ `9512984`，发布版本 `v0.9.5`
验证方式：云端 GitHub Actions 结论 + 下载真实 Release 产物读回 + 本地全量回归

本报告回答一个问题：**本项目的核心目标——用 GitHub Actions 编译、验证并提供下载的独立 sh WASM——是否已经达成。** 结论先行，随后给出逐项证据与仍然有意的缺口。

## 0. 结论

**核心目标已达成。** 需求 R1（实时时钟回调）、R2（网络黑白名单 + 域名通配）、R3（external 注册 + 管道一致工作）三项核心需求均已实现，并在真实 Release 产物上端到端验证；构建、验证、发布全部由 GitHub-hosted runner 完成，不依赖上游组织 runner、发布密钥或 Pyodide/Python。

本轮验证不采信任何“代码看起来实现了”的声明，而是以三类可复核证据为准：

1. **云端结论**：`Standalone Release` 与 `Standalone WASM` 工作流最近一次运行全绿，含三平台 host matrix 与浏览器 Playwright。
2. **真实产物读回**：下载 `v0.9.5` Release 资产，校验摘要、逐文件 SHA256、无 Python 资产、三 target 一致，再用 `nodejs/` loader 实际加载执行 smoke。
3. **本地全量回归**：`cargo test --workspace --locked` 与差分 oracle 套件全绿。

在意缺口（均已在 `SUPPORTED.md` 如实记录，且不违背核心目标）见第 5 节：浏览器无原生进程能力、浏览器同步 XHR 网络路径默认拒绝、`timeout`/`sleep`/`nproc` 为有意桩。

## 1. 从对话抽取的需求

原始输入是一段用户与助手的历史对话。抽离后得到如下基线（已固化在 [项目目标](project-goals.md) 与 [需求设计](design/ai-shell-requirements.md)）：

| 编号 | 用户原始诉求 | 规范化需求 |
| --- | --- | --- |
| 目标 | “用于替换 pwsh 或系统 sh，确保环境一直稳定”；“用 github action 编译 wasm” | 提供 GitHub Actions 编译的独立 sh WASM，抹平宿主差异，供 AI 助手作为 bash 沙箱使用；**不含 WASM Python** |
| R1 | “终端时间只能在启动时传入一个时间，改成 callback，这样无论什么时候 date 都是对的” | 可注入实时时钟：生产 `live` 每次取时调用宿主 callback，测试 `fixed`；覆盖 `date` 与 SigV4；异常不得回落到伪造时间 |
| R2 | “curl 只提供白名单还不支持域名通配，扩展为常见的黑白名单 + 通配” | 结构化网络策略：禁用 / 白名单 / 黑名单 / 组合 / 显式全允许；`*.example.com` 严格子域通配；deny 优先；逐跳重定向校验 |
| R3 | “external 注册接口能把外部可执行程序接入 sh，但外部程序无法使用终端管道” | 宿主注册 external 命令，接通 argv/stdin/stdout/stderr/退出码/超时/取消，与内置命令共用 FD 与管道语义（`\|`、`\|&`、`$?`、`PIPESTATUS`、`pipefail`） |

## 2. 目标一：GitHub Actions 编译独立 WASM

**状态：达成。** 这是用户明确点名的“核心目标”。

### 2.1 云端运行结论

| 工作流 | Run ID | 触发 | 结论 |
| --- | --- | --- | --- |
| Standalone Release | `34689670098` | tag `v0.9.5` | 全绿 |
| Standalone WASM | `34690078437` | push `main` | 全绿 |
| CI | 最近一次 `main` | push `main` | 全绿（含 `oracle` 差分 job） |

`v0.9.5` Release run 的任务链全部成功：Validate release source → Build release candidate → Verify release candidate → Release host consumption（ubuntu-24.04 / macos-15 / windows-2025 三平台）→ Upload tested release artifact → Publish standalone GitHub Release。`main` 的 Standalone WASM run 同样全绿，且 advisory 的 `--all-features` job 也通过。

### 2.2 不依赖上游资源

- 所有 job 使用 `runs-on: ubuntu-24.04` / `windows-2025` / `macos-15` 等 GitHub-hosted runner，匿名拉取源码即可构建，不需要上游组织的私有 Kubernetes runner。
- Pyodide 工作流已收敛为仅 `workflow_dispatch`，默认 push/PR/tag 链路不会触发，也不进入 standalone 产物。
- 工具链全部固定：`wasm-pack 0.15.0`、`wasm-bindgen-cli 0.2.127`、Binaryen/`wasm-opt 117`、Node `v22.22.1`，版本记录在 `tools/standalone/versions.env` 并写入 `build-manifest.json`。

### 2.3 真实产物读回（本轮执行）

下载 `v0.9.5` 资产并独立校验：

| 检查项 | 结果 |
| --- | --- |
| Release 公开、非 draft / 非 prerelease | 是 |
| 归档 SHA256 | `cc2cc06cf972fe19bf8bda475fc33cb8a5ed36c3bb2dcfa779cc1bb062d0f7d8`，与 GitHub 记录的 digest 一致 |
| 归档大小 | 3,913,050 bytes |
| `sha256sum -c SHA256SUMS` | 31 个文件全部 `OK` |
| `verify-package.mjs` | `verified 3 targets without Python/Pyodide runtime assets` |
| 三 target WASM 一致性 | web/nodejs/bundler 均为 3,407,579 bytes，SHA256 同为 `8d0ce9e7a011dec8e555b6c56237aca00ebac2959532e283f4e1df1422d95071` |
| `build-manifest.json` | version=0.9.5、ref=v0.9.5、commit=`e2f2a7e8c2025e5aa64e21a73003978201f6abd3`、run_id=34689670098、`python_runtime_included=false`、`pyodide_runtime_included=false` |
| 干净目录加载 | `node-smoke.mjs`、`external-smoke.mjs`、`bundler-smoke.mjs` 全部通过 |

历史发布连续性也成立：`v0.9.0` 至 `v0.9.5` 六个 Release 均公开可下载，资产名带版本与提交短哈希，摘要可由 GitHub digest 复核。

### 2.4 本地全量回归（本轮执行）

| 命令 | 结果 |
| --- | --- |
| `cargo test --workspace --locked` | 1597 passed / 0 failed（50 个测试目标） |
| `WASMSH_ORACLE=1 cargo test -p wasmsh-testkit --test suite_runner --locked` | 651 passed / 5 feature-gate skipped / 0 failed（656 total） |

差分 oracle 使用真实 GNU bash 5.2.37 逐字节对照 stdout、stderr、退出码，5 个 SKIP 均为既有 feature 门控（如网络用例要求的 feature 未在本目标启用），不存在静默通过。

## 3. 目标二：R1 实时时钟回调

**状态：达成。**

**用户痛点复现**：原实现每次读 `WASMSH_DATE`，未设置时固定 2026-01-01，同一会话内无法取到当前时间。

**实现证据**：

- `crates/wasmsh-utils/src/clock.rs` 定义公共 `ClockProvider`，提供 `now_unix_ms()`（墙钟）与 `monotonic_now_ms()`（单调），并带 `FixedClock`、`SystemClock`、`UnavailableClock` 实现。
- `util_date`（`system_ops.rs`）与 SigV4 时间戳（`net_ops.rs`）**优先**走 `ctx.clock`；仅在没有 provider 时回落到 legacy `WASMSH_DATE`。缺失时钟时返回非零并写诊断 `clock unavailable; install a host callback or set WASMSH_DATE in legacy mode`，**不回落到固定日期**。
- `date` 单次采样一次供所有格式字段复用；`-R`、`-I*`、`-d` 已实现，未知选项显式报错而非静默忽略。
- P1 所述“只在启动时传入”的根因是缺少宿主 callback：`wasmsh-browser` 暴露 `set_clock_callback(Function)` / `clear_clock_callback()` / `set_fixed_time_ms(i64)`，Node worker 默认安装 `() => Date.now()`。
- 单调时钟独立于墙钟：`$SECONDS` / `time` 走 `WorkerRuntime` 的 monotonic 读数（`crates/wasmsh-state/src/lib.rs` 的 `set_monotonic_elapsed_ms`），宿主校时回拨不会延长 deadline。

**真实产物证据**：`node-smoke.mjs` 在下载的 `v0.9.5` WASM 上验证——同一会话同一 Run 内两次 `date` 分别得到 callback 返回的 `1767225599` 与 `1767225600`（跨年界）；`clear_clock_callback()` 后 `date +%s` 非零退出并匹配 `clock unavailable|clock callback`。浏览器 `clock.spec.ts` 另覆盖 `-R`/`-I`/`-d` 与 SigV4。Rust 侧有 `date_samples_one_fixed_clock_value_for_all_format_fields`、`date_without_clock_fails_instead_of_using_a_startup_default` 等单测。

## 4. 目标三：R2 网络黑白名单与通配 / 目标四：R3 external 管道

**状态：均达成。**

### 4.1 R2 网络策略

| 需求点 | 证据 |
| --- | --- |
| 禁用 / 白名单 / 黑名单 / 组合 / 显式全允许 | `NetworkPolicy`（`net_types.rs`）+ `NetworkPolicyConfig` 协议字段；`enabled=false` 时任何列表都不能放行 |
| deny 优先、精确白名单不能覆盖通配黑名单 | `check()` 求值顺序固定：禁用 → URL/协议校验 → **deny 命中即拒绝** → allow → default_action |
| `*.example.com` 严格多层子域、不含根域、不误配伪后缀 | 规则匹配用标签边界（`target.host.ends_with(".example.com")` 且长度大于根域），`badexample.com`/`example.com.evil.test` 不匹配 |
| 不支持的模式（`api.*.com`、`foo*`、`?`、CIDR）配置期报错 | `parse_rules` 返回 `only '*' and '*.example.com' wildcards are supported` 等错误，初始化失败而非静默当精确域名 |
| 规范化：小写、尾点、IDNA、IPv6、隐含/显式端口 | `NormalizedTarget` 统一处理，`https://example.com` 与 `:443` 判定一致 |
| 逐跳重定向校验、跨源剥离敏感头 | `fetch_with_redirects` 手动跟随、每跳重检目标；跨源移除 `Authorization`/`Cookie`/`Proxy-Authorization`/`Host`，跨源 SigV4 拒绝 |
| 旧 `allowed_hosts` 兼容 | 映射为 allow-only 白名单模式；同时传新旧配置报错 |
| 浏览器路径失败封闭 | 同步 XHR 无法在请求前逐跳阻断，故默认拒绝；仅在 `set_trusted_network_broker(true)` 且接入可信 broker 后启用 |

**真实产物证据**：`node-smoke.mjs` 起本地可控 HTTP fixture，断言允许站点的相对重定向成功且请求计数为 2；跨端口重定向的目标服务器命中数为 **0**（不是 CORS 报错的假象）；`--max-filesize` 触发响应过大；非法策略 `init` 失败且后续请求计数保持 4（失败封闭）；禁用网络后拒绝且计数不变。`cargo test -p wasmsh-utils net_types` 覆盖真值表与规范化。

### 4.2 R3 external 与管道

| 需求点 | 证据 |
| --- | --- |
| 宿主注册 / 注销 / 查询 | `WasmShell.register_external` / `unregister_external` / `external_commands`；未注册 127 |
| 固定可执行文件，argv 不二次解释 | 注册固定 `executable`，传入已解析 argv（含 argv[0]），**不使用** `sh -c` / `shell: true`；smoke 断言 `'arg with spaces'`、`'x;y'`、`'$HOME'` 原样传递 |
| 有限 I/O | `spawnSync` 式 executor，stdin EOF、二进制传参、同时排空 stdout/stderr |
| 流式管道、背压、早退、取消 | `ExternalProcess` trait + `PendingStreamingPipeline`，由 `StartRun`/`PollRun` 驱动；Node `createNodeExternalStreamExecutor` 用有界队列与 pause/resume |
| shell 语义 | `<`、`>`、`>>`、here-doc/here-string、`\|`、`\|&`、命令替换、`$?`、`PIPESTATUS`、`pipefail` 共用同一 FD 路由 |
| 退出码分类 | 124=超时、125=限额、126=启动失败/宿主不支持、127=未注册；正常退出保留自身状态 |
| 资源上限 | 输入/输出字节上限、timeout、stream_queue_bytes；runtime 侧 wall-clock deadline |
| 浏览器边界 | 无原生 executor 时明确返回“native process unsupported”，status 126 |

**真实产物证据**：`node-smoke.mjs` 在真实 WASM 上验证 `printf hi | hostcat | wc -c` → `2`；`hostcat <<'EOF'` / `< /binary` 二进制往返 `[0,1,255]`；`hostemit 2>&1 > f` 得到 `ERR\nOUT\n`、`hostemit > f 2>&1` 得到 `OUT\nERR\n`（左到右 FD 复制不同）；`hoststatus 7 | hostcat` 的 `PIPESTATUS` → `7 0`，`pipefail` → 退出 7；流式 `hostproducer | head -c 1` → `P`、`yes | hostcat | head -n 1` → `y`、8192 字节大输入、`hostdual` 双路 64 KiB、`hostdual |& hostcat` 合并；超时 124、启动失败 126、取消后 130 且会话可继续。`external-smoke.mjs` 用原生跨平台测试程序覆盖 argv/EOF/双流/env/cwd/路径映射/状态/超时/限额。

## 5. 尚未达成或有意的缺口

以下均**不违背**本项目核心目标（给 AI 提供稳定 bash 沙箱），且已在 `SUPPORTED.md` 如实标注；列出以免把“已达成”误读为“完整 GNU Bash + 完整浏览器能力”：

1. **浏览器无原生进程能力**：浏览器 external 只能注册/查询，启动原生可执行返回 126。这是安全边界而非缺陷——浏览器没有受控的宿主进程模型。
2. **浏览器同步 XHR 网络路径默认拒绝**：无法在请求发出前逐跳校验重定向，因此失败封闭；需接入可逐跳检查的可信 broker 才启用。这是相对“能联网”的能力缩减，方向正确。
3. **确定性桩**：`timeout` 永不执行命令（恒 125）、`sleep` 立即返回、`nproc` 恒 1。沙箱无真实墙钟/进程模型，属有意设计，已在文档标注。
4. **少量 Bash 差异**：`${arr[@]:1:-1}` 负长度切片、无 `awk getline`、`readonly` 失败不中止脚本、`$((1/0))` 与深层递归更宽容（bash 会 abort）。方向上是“更宽容”，不静默算错，已在 `SUPPORTED.md` 记录。
5. **交付渠道**：第一期只要求 GitHub Release，未发布 npm/crates.io/PyPI/GHCR；核心目标未要求这些。

## 6. 复核命令

```sh
# 1. 云端结论
gh run list --workflow=release.yml --limit 3
gh run view 34689670098 --json jobs

# 2. 真实产物读回
gh release download v0.9.5 --pattern '*.tar.gz'
sha256sum wasmsh-standalone-0.9.5-*.tar.gz   # 期望 cc2cc06c…
tar -xzf wasmsh-standalone-0.9.5-*.tar.gz
cd wasmsh-standalone-0.9.5-e2f2a7e8c202
node ../../../tools/standalone/verify-package.mjs .
sha256sum -c SHA256SUMS
cd ../../..
node tools/standalone/node-smoke.mjs target/verify-r095/wasmsh-standalone-0.9.5-e2f2a7e8c202
node tools/standalone/external-smoke.mjs
node --experimental-wasm-modules tools/standalone/bundler-smoke.mjs target/verify-r095/wasmsh-standalone-0.9.5-e2f2a7e8c202

# 3. 本地回归
cargo test --workspace --locked
WASMSH_ORACLE=1 cargo test -p wasmsh-testkit --test suite_runner --locked
```

## 7. 证据来源

- 需求与边界：[项目目标](project-goals.md)、[需求设计](design/ai-shell-requirements.md)、[构建计划](guides/standalone-wasm-build-plan.md)
- 实际能力与差异：[SUPPORTED.md](../SUPPORTED.md)
- 逐阶段实施与发布记录：[实施跟踪](implementation-tracker.md)
- 本轮发布运行：`Standalone Release` run `34689670098`；`Standalone WASM` run `34690078437`
