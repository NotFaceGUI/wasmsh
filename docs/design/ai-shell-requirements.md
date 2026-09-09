# AI Shell：需求分析与接口设计

状态：待实施设计。范围与基线见 [项目目标](../project-goals.md)。本文中的新增配置、接口和错误分类均为拟议契约，不是当前可用 API。

## 1. 源码核对与根因边界

| 位置 | 已核对行为 | 结论 |
| --- | --- | --- |
| [`system_ops.rs`](../../crates/wasmsh-utils/src/system_ops.rs)，`util_date` | 每次读取 `WASMSH_DATE`；默认固定日期；`-d` 跳过参数，`-R/-I` 仅接受 | 不是只能在 Init 传时间，但现有接口没有实时取时能力；部分选项还会静默忽略 |
| [`net_ops.rs`](../../crates/wasmsh-utils/src/net_ops.rs)，`sigv4_timestamps` | AWS 签名时间也从 `WASMSH_DATE` 读取 | 只修 date 显示会遗漏请求签名 |
| [`wasmsh-state`](../../crates/wasmsh-state/src/lib.rs)，`seconds_value` | 原生使用 Instant；wasm32 返回 `0` | 时长与墙上时间是两种能力，不能混用 |
| [`net_types.rs`](../../crates/wasmsh-utils/src/net_types.rs)，`HostAllowlist` | 精确域名、`*.` 子域后缀、IP、端口；空列表拒绝；仅 HTTP(S) | 子域通配已经存在，黑名单和显式默认策略缺失 |
| 同上，`split_port` / `check` | 手工拆分规则端口；URL 端口使用 `parsed.port()` | 规则校验、IPv6 和默认端口规范化需补测；默认 80/443 应按有效端口匹配 |
| [`net_ops.rs`](../../crates/wasmsh-utils/src/net_ops.rs)，`fetch_with_redirects` | 将 `follow_redirects` 设为 false，由 Rust 检查下一跳 | 依赖宿主真正禁止自动跳转；跨源跳转前还需处理敏感请求头 |
| [`worker.js`](../../e2e/standalone/fixture/worker.js)，`wasmsh_http_fetch` | 同步 XHR，未使用 `followRedirects` 参数 | 示例桥接没有兑现 Rust 所需的逐跳控制，须实测并修复实际交付宿主 |
| [`wasmsh-runtime`](../../crates/wasmsh-runtime/src/lib.rs)，`ExternalCommandHandler` | `(name, argv, Option<ExternalCommandStdin>) -> Option<ExternalCommandResult>`；stdin 可读，输出为两个 Vec | 已有有限 I/O 通路；不是 OS 进程注册器，也不是双向流式 API |
| 同上，`BufferedPipeProcess` | 非流式阶段输入暂存 VFS，输入结束后执行并收集输出 | external 等待上游 EOF、输出等待回调返回，不能据此保证无限输入或早退消费正常 |
| [`runtime_protocol.rs`](../../crates/wasmsh-runtime/tests/runtime_protocol.rs) / [`shared_io.rs`](../../crates/wasmsh-runtime/tests/shared_io.rs) | 已有 `printf hi \| hostcat`、输入/输出重定向、stderr 重定向顺序测试 | 复用并扩展既有用例；回调测试不等于原生子进程端到端测试 |
| [`wasmsh-browser`](../../crates/wasmsh-browser/src/lib.rs)，`WasmShell` | 导出 Init/exec/文件操作/cancel/signal，没有 external 或 clock 注册方法 | standalone 的可消费宿主 API 需要补齐 |
| [`wasmsh-protocol`](../../crates/wasmsh-protocol/src/lib.rs) | 已有 StartRun/PollRun/Yielded，暂无 external 请求/完成消息 | 可以复用渐进执行入口，但不能假设它能暂停任意同步回调 |

表中为静态源码证据。历史接入方和构建版本未知，不能宣称已经复现用户的 external 故障。后续必须记录最小失败脚本、实际加载 WASM 的摘要和宿主注册方式。

## 2. 需求 R1：实时可注入时钟

### 2.1 行为契约

- 生产 `live` 模式：每次命令需要“现在”时调用宿主 callback。同一个 `exec` 内的两次 `date` 也必须重新读取，不能只在 shell 初始化或每个 Run 开头刷新变量。
- 提供 `fixed` 测试模式，显式设置时间；同样输入得到同样日期。真实时间正确的前提是宿主时钟正确，沙箱不自行校时。
- callback 返回 UTC Unix epoch 毫秒。Rust 接口使用有符号整数；JS 传入值须为有限安全整数且在日期实现支持范围内，不能静默截断 NaN/Infinity 或秒毫秒混淆。
- 单次 `date` 只采样一次，所有格式字段使用同一个值；SigV4 单次签名也只采样一次，避免跨日拼出不一致日期。
- 默认 UTC；`date +%s`、`date -u` 和默认文本使用同一时间源。需要其他时区时使用显式配置，不读取宿主 locale 作为隐藏输入。
- callback 缺失、抛异常、返回非法值时返回非零命令状态和诊断，不回落到 2026 年的固定时间。适配器在建会话时安装 live callback；旧低层接口的固定模式保留为显式兼容入口。
- `WASMSH_DATE` 作为旧行为保留在 legacy 模式；生产 live 模式不允许这个可被脚本修改的变量覆盖共享时钟。若需支持 `date -d`，仅改变该命令的显式日期输入，不改变签名时钟。
- `-d/-R/-I` 当前的静默忽略必须消除：实现已声明支持的语义，或明确返回“不支持选项”。不把“取时正确”误写为完整 GNU date 兼容。

### 2.2 接口与接入

建议公共 `ClockProvider` 提供 `now_unix_ms() -> Result<i64, ClockError>`；能力由 `WorkerRuntime` 持有，并传递到工具执行上下文。不要把 JS 类型放进纯 Rust 工具层。

WASM/JS 层新增会话级 callback 注册，正常 JS 适配器使用 `() => Date.now()`；回调注册在执行该 WASM 的 worker 中。函数不能通过 JSON/`postMessage` 直接传递，跨 worker 场景需由启动代码安装或按宿主协议转发。生产时间 callback 可同步返回，因此无需为这一项单独重写异步执行器。

clock 能力应覆盖 `util_date` 和 `sigv4_timestamps`，并盘点 `$SECONDS`、`time`、文件时间戳。`$SECONDS`、执行超时和性能计时使用独立单调时钟；系统校时回拨不能使超时失效。文件时间戳不在本轮强行改成真实时间，但要标明其现行语义。

会话重置后重新建立单调时间起点；共享或复制子 shell 状态时继续使用同一宿主时钟能力。禁止回调重入同一个 `WasmShell.exec`，异常需在绑定边界转换为诊断。

### 2.3 关键验证

使用可推进 fake clock，验证同会话多 Run、同 Run 多 date、命令替换、跨午夜/年界和闰日。补一项真实宿主延迟后的 date 对照，误差按测试容差判断；不要仅依赖易抖动的 sleep 测试。SigV4 断言实际签名时间与回调采样一致。

## 3. 需求 R2：网络黑白名单与域名通配

### 3.1 配置模型

建议新增结构化 `NetworkPolicy`，沿用仓库配置的 snake_case 风格：

```json
{
  "network_policy": {
    "enabled": true,
    "default_action": "deny",
    "allow": ["example.com", "*.example.com:443"],
    "deny": ["blocked.example.com", "*.restricted.example.com"]
  }
}
```

| 常见模式 | 配置语义 |
| --- | --- |
| 关闭网络（默认） | `enabled: false`，其余列表不能使请求放行 |
| 白名单 | enabled，`default_action: deny`，allow 中的目标可访问 |
| 黑名单 | enabled，`default_action: allow`，deny 中的目标不可访问 |
| 黑白名单组合 | enabled，`default_action: deny`，allow 放行范围减去 deny |
| 显式全部允许 | enabled，`default_action: allow`，deny 为空；仍遵守协议/资源限制 |

固定决策顺序：禁用检查 -> URL/协议校验 -> deny 命中拒绝 -> allow 命中允许 -> default_action。**deny 始终优先，精确白名单不能覆盖通配黑名单。**

策略由可信宿主配置，不从 shell 环境变量读取。不提供脚本修改权限策略的命令。首版策略在会话创建时确定；未来热更新必须原子替换并规定在途请求处理方式。

兼容入口 `allowed_hosts` 映射到白名单模式，空列表仍禁止联网。同时传新旧配置应报错，避免忽略某一组限制。非法 JSON、非法规则和未知 default_action 应使初始化失败，不使用 `unwrap_or_default` 隐藏配置错误。

### 3.2 通配和规范化规则

| 规则 | 匹配要求 |
| --- | --- |
| `example.com` | 仅该域名，不包含子域 |
| `*.example.com` | 一个或多个子域层级，例如 `a.example.com`、`a.b.example.com`；不含根域 |
| `*.example.com:443` | 上述子域且 URL 有效端口为 443 |
| `*` | 任意合法 HTTP(S) 主机；allow/deny 两侧含义一致，必须显式填写 |
| IPv4 / `[IPv6]:port` | 规范化后的精确地址及可选端口；不把 IPv6 尾段误解析为端口 |
| `api.*.com`、`foo*`、`?`、正则、CIDR | 首版不支持，返回配置错误；不静默当作精确域名 |

白/黑名单使用同一 matcher。规则与请求双方统一小写、尾点、IDNA/Punycode 和 IP 表示；请求使用现有 `url::Url` 解析，规则使用结构化 host/port 解析。仅以主机标签边界比较后缀，`badexample.com`、`example.com.evil.test` 不能匹配 `*.example.com`。

端口按 `port_or_known_default()` 对应的有效值匹配：`https://example.com` 与 `https://example.com:443` 对 `example.com:443` 的结果一致。无端口规则匹配任何有效端口；端口约束不是协议约束，HTTP(S) scheme 另行检查。首版拒绝含 userinfo 的 URL，避免凭据和主机识别歧义。

### 3.3 真实网络路径约束

1. `curl` 和 `wget` 共用策略；网络 backend 的每个实际请求入口都校验，不能只在参数解析时检查一次。生产 backend 不能继承默认放行的 `check_url` 而漏检。
2. 重定向逐跳重新判断目标，包含相对 Location、跨端口/协议、循环与最大跳数；底层 HTTP 客户端关闭自动跟随。拒绝跳转发生在请求发出前。
3. 当前 standalone 示例的 XHR 自动重定向路径需要替换：JS 服务端宿主使用可控客户端；纯浏览器若无法逐跳观察跨源跳转，应拒绝重定向，或通过可信 broker 执行策略。检查最终 URL 只能事后发现，不能防止已经发生的外发请求。
4. 跨 origin 重定向移除 Authorization、Cookie、Proxy-Authorization 等敏感头；签名请求需要按新目标重新签名或拒绝，不能原样携带旧签名。
5. 配置全局超时、连接超时、最大跳数、响应体字节限制，并在宿主读取过程中强制执行。当前 `HttpRequest` 的部分限额只是 advisory，完整分配后才检查不足以限制内存。
6. 域名黑白名单不等于 IP 隔离。内网/回环/链路本地与 DNS rebinding 防护由有 DNS/socket 控制能力的宿主 broker 承担，并在连接目标层校验；浏览器不能宣称拥有该能力。是否允许内网作为独立显式能力，不能仅凭一个公网域名规则推导。
7. external 原生程序的网络访问不会自动经过内置 curl 的策略；宿主若授予这类程序，必须分别限制其权限或明确披露其能力。

拒绝诊断应包含规范化目标、拒绝类别及可定位规则，隐藏 URL 凭据、敏感查询和认证头。保留非零退出码，并向宿主提供可区分的策略拒绝、非法 URL、DNS/连接错误、超时、响应过大等类别，避免 AI 将拒绝误判为暂时网络故障而循环重试。

## 4. 需求 R3：external 与 Bash 管道一致工作

### 4.1 先明确两层能力

当前 Rust 接口提供“按命令名称处理的回调”，不直接创建系统进程。`argv` 包含 `argv[0]`；原生 spawn 适配器需要按 API 约定剥离一次，防止丢参或重复程序名。

已有 `ExternalCommandStdin` 只能证明输入可读；输出需要一次性返回 `Vec<u8>`，通用管道阶段还会等待输入 EOF。这对有限过滤命令可行，但无法保证 `yes | external | head`、无限生产者或边读边写场景。修复分为有限兼容和完整流式两步，后者属于最终验收范围。

### 4.2 有限输入的兼容接入

- 在独立 WASM 宿主层导出 external 注册/注销能力，并让命令查询能反映已注册工具；不依赖 Pyodide 的 Python handler。
- 命令注册表将固定命令名映射到固定可执行程序/可信 handler。传递已解析 argv，禁止再次拼接成 `pwsh -Command`、`sh -c` 或 `shell: true`。
- 正确传递 stdin 字节，发送完毕关闭子进程 stdin；未提供输入时视为 EOF，不能挂接宿主交互终端一直等待。
- 同时排空子进程 stdout/stderr，避免只读其中一个导致系统管道塞满；结果回到 runtime 的统一 FD 路由，不能直接打印到宿主控制台。
- stdout/stderr 保持二进制；仅在 AI 文本展示边界解码，不能先转字符串再传下一段管道。
- 宿主提供命令级 cwd 映射、导出环境白名单、时间/输出上限；VFS 中的路径不自动成为原生程序可访问文件，必须通过受控映射或显式文件交换解决。
- 未注册返回 127；已注册但无法启动对应 126；进程正常退出保留其状态。信号、超时与取消使用统一宿主分类及可移植映射，不能把 Windows 退出值无条件当 POSIX 信号。

旧同步接口可保留为有限输入适配器，但必须带容量上限并声明无实时输出。Node 同步 spawn 可用于有限兼容场景；不能把返回 Promise 的 handler 直接塞进当前同步 Rust callback。

### 4.3 流式进程协议

建议引入 external 进程能力，最小操作为 start、write_stdin、close_stdin、poll/read_stdout、poll/read_stderr、wait/status、cancel。命名可按现有 Rust 风格调整，但需要以下语义：

- 每个进程以会话 ID、run ID、command ID 定位，拒绝旧运行的迟到结果；每个流有明确 EOF，暂时无数据不等于 EOF。
- 双向 I/O 分块传输并使用有上限的队列；慢消费者产生背压，写入暂停不能变成截断丢字节。
- 外部进程与内置命令都纳入现有 PipeBuffer/调度过程。stdout 就绪即可送给下游，不等待进程退出；stderr 独立排空，只有 `2>&1` 或 `|&` 才合并。
- 消费者提前结束时关闭对应读端，并传播关闭/取消至上游；原生进程无需产生无限数据才能获知下游已经退出。保留管道各段退出状态及 `pipefail` 语义。
- 阻塞的 native handler 不得占住负责转发 I/O 与取消的同一个 JS 事件循环。通过 worker/宿主服务或可挂起执行协议桥接；JS Promise 只有 runtime 已能 yield/resume 时才能接入。
- 现有 StartRun/PollRun 可以作为扩展入口，但需要验证运行中间位置能暂停和恢复。只在整条命令完成之后 Yielded，不能解决 external 阻塞。
- 超时/取消需要关闭所有流、终止并回收子进程及其受管理子进程树、删除暂存文件。宿主进程回收与 shell 内部 signal/trap 是不同职责。
- output/pipe/暂存 VFS/宿主进程四处资源都有上限。step budget 不覆盖外部程序运行时间，必须额外实施 wall-clock deadline。

浏览器无原生进程能力时只注册可信 JS 工具或显式远程工具；对原生 executable 注册返回“不支持宿主能力”，不能展示成已经支持本地进程。

### 4.4 必须保持的 shell 语义

stdin/stdout/stderr 使用与 builtin 相同的 FD 模型；重定向按从左到右应用，`external 2>&1 >out` 与 `external >out 2>&1` 必须不同。覆盖 `<`、`>`、`>>`、here-doc、here-string、`|`、`|&`、命令替换、`$?`、`PIPESTATUS` 和 `pipefail`。

“管道”指标准流组合，首版不包括 PTY、终端尺寸、光标控制、交互式 TUI 和完整作业控制。shell 保持稳定，并不保证任何需要真实终端的宿主程序都可使用。

## 5. 模块改造顺序

1. 在 `wasmsh-runtime`/`wasmsh-utils` 定义共享 ClockProvider/NetworkPolicy 能力，复用 `UtilContext` 和 `NetworkBackend`，避免同一条策略在多个宿主中各自演化。
2. 扩展 `wasmsh-browser` 和独立 JS 宿主适配层，补配置验证、回调注册、资源限制和能力查询。协议数据可以序列化，函数与进程句柄不能直接写入 JSON。
3. 网络规范化与规则匹配先用表驱动测试；随后验证真实 HTTP broker 的重定向、超时和实际发包记录。
4. external 先贯通已有有限 I/O 路径，再扩展可挂起进程状态与 PipeBuffer，保持旧 Rust handler 可通过受限适配器工作。
5. 协议新增字段按兼容策略版本化；同步更新 standalone 加载器、类型声明、示例与测试。保留旧接口必须显式说明旧行为，不能同名悄悄放宽网络权限。

## 6. 验收矩阵

以下是待实现测试，测试夹具名称如 `hostcat`、`hostemit` 不代表现有内置命令。

| ID | 场景 | 必须观察到的结果 |
| --- | --- | --- |
| T01 | 同会话两次 Run，中间推进 fake clock | 两次 `date +%s` 对应各自当前时间，无重新 Init |
| T02 | 一次 Run 中 `date +%s; date +%s` | callback 返回不同采样时，输出分别对应；命令替换同样成立 |
| T03 | fixed 模式、跨日、回调异常/非法数字 | 固定值可复现；跨日字段一致；异常非零，不返回默认日期 |
| T04 | date 与 AWS SigV4 | 各自使用共同 provider；签名中日期和时间来自一次采样 |
| T05 | 单调计时与宿主墙钟回拨 | `$SECONDS` 按已声明语义增长；deadline 不被墙钟回拨延长 |
| N01 | 默认配置、空白名单、网络禁用 + allow `*` | 全部拒绝，真实网络调用计数为 0 |
| N02 | `*.example.com` 与根域/多层子域/伪后缀 | 只匹配严格子域；规则两侧行为一致 |
| N03 | allow `*.example.com` + deny `blocked.example.com` | 被 deny 的域名始终拒绝；顺序不影响结果 |
| N04 | 黑名单模式、显式全允许、deny `*` | default_action 与 deny 优先级符合真值表 |
| N05 | 大小写、尾点、IDNA、IPv6、隐含 443/显式 443、非默认端口 | 相同规范目标得到同一判定；非法规则初始化失败 |
| N06 | `file:`/`ftp:`、含 userinfo、无 host、非法 JSON | 不发包，返回可识别配置或 URL 错误 |
| N07 | 允许站点跳到拒绝站点，curl 有/无 `-L`，wget | 被拒目标服务器命中为 0；无需 -L 也不能自动偷跑跳转 |
| N08 | 跨 origin、循环跳转、超大响应、慢连接 | 敏感头不泄漏；跳数、内存、时间限额生效 |
| E01 | `printf hi \| hostcat \| wc -c` | 输出计数 2，external 原生/JS 宿主链路均参与 |
| E02 | `hostcat < /in > /out`、追加、here-doc/here-string | VFS 内容、换行与输入 EOF 正确 |
| E03 | `hostemit 2>&1 > /out` 与 `hostemit > /out 2>&1` | stderr 去向符合左到右 FD 复制语义 |
| E04 | `hostemit \|& hostcat`、命令替换 | 流合并正确；命令替换仅按 Bash 语义去尾部换行 |
| E05 | 含 NUL/非 UTF-8/CRLF 的输入与多段 external | 原始字节完全一致；测试在字节接口比较 |
| E06 | 中间命令非零、未注册、启动失败、pipefail | `$?`、PIPESTATUS、&&/|| 与状态映射正确 |
| E07 | 同时大量 stdout/stderr，大输入大于管道容量 | 无死锁、无丢失；缓冲与暂存上限可观察 |
| E08 | `hostproducer \| head -c 1`、`yes \| hostcat \| head -n 1` | 流式模式及时输出并收尾，不等待无限 EOF，不遗留进程 |
| E09 | 执行中取消/超时、接着运行新命令 | 全部进程/流回收；旧结果不进入新 Run；会话可继续或明确标为已销毁 |
| E10 | 带空格/引号/$/分号的 argv、cwd/env、未映射 VFS 路径 | 参数不二次解释；环境受限；未授予路径不暗中落到宿主 |

时间/策略/有限回调的 Rust 测试只是第一层。最终必须使用 Actions 产出的 WASM 和真实宿主适配器跑端到端测试；网络测试通过本地可控服务器记录实际收到的请求，不依赖公共网站在线状态。

## 7. 参考依据

- 本仓库 [沙箱现状说明](../reference/sandbox-and-capabilities.md) 与 [网络能力 ADR](../adr/adr-0021-network-capability.md)；本设计提出后续变化，不应将两者混为同一状态。
- [MDN：XHR responseURL](https://developer.mozilla.org/en-US/docs/Web/API/XMLHttpRequest/responseURL)：该值是重定向后的最终 URL。结合当前桥接代码，推断仅校验返回 URL 无法实现请求前的逐跳阻断；需端到端验证。
