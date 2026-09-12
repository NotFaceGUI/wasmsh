# wasmsh

## 项目目标

面向 AI 助手提供统一的 Bash 兼容沙箱，减少对宿主 PowerShell / sh 和系统工具环境的依赖。主交付物为 GitHub Actions 编译的独立 sh WASM，不包含 WASM Python / Pyodide。

- [项目目标、范围与里程碑](docs/project-goals.md)
- [时间回调、网络黑白名单与 external 管道需求](docs/design/ai-shell-requirements.md)
- [独立 WASM 的 GitHub Actions 构建计划](docs/guides/standalone-wasm-build-plan.md)

以上文档描述目标、边界和验收证据；实际已实现能力以
[`SUPPORTED.md`](SUPPORTED.md) 与实施跟踪为准。当前主交付是独立 standalone
sh WASM；Python、集群部署和发布渠道属于可选 profile，不会进入
standalone 产物。

## Standalone quick start

使用已解包的 standalone artifact 运行 Node 接入示例：

```sh
node examples/standalone/node.mjs path/to/wasmsh-standalone-<version>-<commit>
```

示例加载 `nodejs/` 产物，安装实时 host clock，验证二进制 VFS，并通过
`shell: false` 的固定 external 注册传递 stdin。完整的 Node、browser、bundler
API 和网络 broker 约束见
[Standalone WASM embedding](docs/guides/standalone-embedding.md)。

**Bash-compatible shell runtime in Rust, compiled to WebAssembly.** The primary
delivery is the standalone sh WASM built by this repository's GitHub Actions;
browsers, Pyodide, and the Kubernetes sandbox pool are optional profiles and
are not part of the standalone artifact.

[![License](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)
[![standalone WASM](https://img.shields.io/badge/build-standalone%20wasm-blue)](.github/workflows/wasm-build.yml)

## What it is

A sandbox for AI agents that need a Bash-compatible shell and a virtual
filesystem without giving the script implicit host access. The primary
standalone delivery runs the shell in WebAssembly with 88 utilities, binary
events, no Python runtime, no native processes by default, and no network
unless a host capability is explicitly installed. Python/Pyodide remains a
separate optional profile.

Bash compatibility is a **verified subset**, not an open-ended promise: the
suite in [`tests/suite/differential/`](tests/suite/differential/) runs scripts
through both wasmsh and a real `bash` and asserts identical stdout, stderr,
and exit status, with a dedicated `oracle` CI job. What is verified, what
diverges, and what is only stubbed is listed in [`SUPPORTED.md`](SUPPORTED.md).

Primary and legacy deployment modes from one core:

| Target | When | Entry point |
|-|-|-|
| **Standalone** (`wasm32-unknown-unknown`) | browser Web Worker, offline | [`crates/wasmsh-browser`](crates/wasmsh-browser/) |
| **Pyodide** (`wasm32-unknown-emscripten`) | Node or browser, Python sharing the VFS | [`packages/npm/wasmsh-pyodide`](packages/npm/wasmsh-pyodide/) |
| **Scalable** (Kubernetes) | multi-tenant agent platforms | [`deploy/helm/wasmsh`](deploy/helm/wasmsh/) |

## Why it's a good fit for Deep Agents

### Secure by construction

LLM-generated shell commands are adversarial input. wasmsh is built so a bad `rm -rf /` or a curl to an exfil host cannot escape the sandbox:

- **WASM boundary.** The standalone WASM core has no syscalls or `std::fs`.
  Node native processes are an explicit, fixed registration through the
  shipped host adapter, never an implicit shell command.
- **Capability-based VFS.** Every session gets an isolated in-memory filesystem; nothing on the host is visible unless the embedder mounts it.
- **Network allowlist.** `curl` / `wget` route through a host-mediated broker that enforces a per-session hostname allowlist. Empty list = no network.
- **Step budgets.** Every command runs with a bounded step count; runaway loops and fork-bombs terminate deterministically.
- **Per-session V8 isolation** (scalable path). Each session is its own worker with a capped heap; one session cannot starve its neighbours.
- **Clean-room provenance.** No GPL code in the core — behavior-compatible with bash, but a fresh implementation, so no licence contamination for downstream embedders.

Full surface in [docs/reference/sandbox-and-capabilities.md](docs/reference/sandbox-and-capabilities.md); threat model and design choices in [docs/explanation/design-decisions.md](docs/explanation/design-decisions.md).

### Fast and dense

The standalone core needs no container or VM and starts as a WASM instance;
optional Node external commands are separate host processes only when the
caller registers them:

- **~300 ms cold spawn**, **~6 ms snapshot restore** once the template worker is warm
- **~1.5 ms** per warm bash command, **~3 ms** per warm `python3 -c` round-trip through the dispatcher
- **~80 MB RSS** per active session in steady state (stock Pyodide + bash)

That makes per-node density the ceiling instead of CPU. Rough sizing on a 64 GB / 40-core node: **~500–800 warm sessions** for typical agent workloads, **~100 session creates/s** burst throughput. See [docs/guides/performance-testing.md#sizing-a-scalable-deployment](docs/guides/performance-testing.md#sizing-a-scalable-deployment) for the benchmark and full capacity table — and re-run `just bench-dispatcher-compose` on your own hardware before committing to a size.

## Use with LangChain Deep Agents

Two interchangeable backend classes with the identical `BaseSandbox` surface — upgrading from laptop to cluster is a one-line import change:

| Ecosystem | Package | In-process | Scalable |
|-|-|-|-|
| npm | [`@mayflowergmbh/langchain-wasmsh`](packages/npm/langchain-wasmsh) | `WasmshSandbox.createNode()` | `WasmshRemoteSandbox.create({ dispatcherUrl })` |
| Python | [`langchain-wasmsh`](packages/python/langchain-wasmsh) | `WasmshSandbox()` | `WasmshRemoteSandbox(dispatcher_url)` |

```typescript
import { createDeepAgent } from "deepagents";
import { WasmshSandbox } from "@mayflowergmbh/langchain-wasmsh";

const sandbox = await WasmshSandbox.createNode();
const agent = createDeepAgent({ backend: sandbox });
```

```python
from deepagents import create_deep_agent
from langchain_wasmsh import WasmshSandbox

agent = create_deep_agent(backend=WasmshSandbox())
```

The Python adapter also ships a **persistent Python REPL middleware**
(`WasmshInterpreterMiddleware`) with optional programmatic tool calling
(PTC) — the model can `await tools.<name>(...)` inside one `py_eval`
invocation to fan out, branch, and chain LangChain tools without
extra LLM turns:

```python
from langchain_wasmsh import WasmshInterpreterMiddleware

agent = create_deep_agent(
    model="claude-sonnet-4-6",
    tools=[my_search_tool],
    middleware=[WasmshInterpreterMiddleware(ptc=["my_search_tool"])],
)
```

A `WasmshFilesystemBackend` exposes the same sandbox VFS as a DeepAgents
[Memory](https://docs.langchain.com/oss/python/deepagents/memory)
backend; pair with `SkillsMiddleware` for `import skills.<name>` support
inside the REPL.

Full integration guide (remote variant, deployment topology, operational knobs): [docs/integrations/langchain-wasmsh.md](docs/integrations/langchain-wasmsh.md).

Runnable examples covering every deployment shape:

| Variant | Directory |
|-|-|
| In-browser agent (Pyodide + LLM all client-side) | [`examples/deepagent-browser/`](examples/deepagent-browser/) |
| In-process Node agent | [`examples/deepagent-typescript/`](examples/deepagent-typescript/) |
| In-process Python agent | [`examples/deepagent-python/`](examples/deepagent-python/) |
| Scalable Docker Compose (remote sandbox) | [`examples/deepagent-typescript/`](examples/deepagent-typescript/#remote-setup-docker-compose) + [`examples/deepagent-python/`](examples/deepagent-python/#remote-setup-docker-compose) |
| Scalable Kubernetes (Helm) | [`examples/deepagent-kubernetes/`](examples/deepagent-kubernetes/) |
| Direct Rust embedding (no sandbox layer) | [`examples/rust/`](examples/rust/) |
| Raw wasm-pack (JS/TS, no LLM) | [`examples/web/`](examples/web/), [`examples/typescript/`](examples/typescript/), [`examples/python/`](examples/python/) |

## Install

The primary deliverable is the standalone sh WASM artifact produced by
[`.github/workflows/wasm-build.yml`](.github/workflows/wasm-build.yml). A
successful run uploads `wasmsh-standalone-<version>-<commit>` with the three
loaders, type declarations, host adapters, license, `VERSION`, and a SHA256
manifest; tagged `v*` builds publish the same verified archive as a Release.

Build locally with `bash tools/standalone/build.sh dist/standalone-pkg` (needs
the pinned wasm-pack, wasm-bindgen-cli, and Binaryen 117 from
[`tools/standalone/versions.env`](tools/standalone/versions.env)) or run
`just ci` for the Rust checks.

The registry packages below (`crates.io`, npm, PyPI, container images) belong
to the optional Pyodide/Kubernetes profile and are **not** published by the
standalone build; do not expect the standalone shell there.

| Registry | Package | Install |
|-|-|-|
| crates.io | `wasmsh-runtime` | `cargo add wasmsh-runtime` |
| npm | `@mayflowergmbh/wasmsh-pyodide` | `npm i @mayflowergmbh/wasmsh-pyodide` |
| PyPI | `wasmsh-pyodide-runtime` | `pip install wasmsh-pyodide-runtime` |
| Containers | `ghcr.io/mayflower/wasmsh-{dispatcher,runner}` | `docker pull` |

## Docs

| | |
|-|-|
| **Start here** | [Tutorials](docs/tutorials/index.md): [Rust](docs/tutorials/getting-started.md), [JavaScript](docs/tutorials/javascript-quickstart.md), [Python](docs/tutorials/python-quickstart.md) |
| **Deep Agents** | [Integration guide](docs/integrations/langchain-wasmsh.md) (in-process + remote, both languages) |
| **Deploy** | [Docker Compose](deploy/docker/README.md) (single-host), [Helm chart](deploy/helm/wasmsh/README.md) (Kubernetes), [snapshot-runner architecture](docs/explanation/snapshot-runner.md), [runner runbook](docs/how-to/runner-runbook.md) |
| **Tune** | [Performance testing & sizing](docs/guides/performance-testing.md), [dispatcher API](docs/reference/dispatcher-api.md), [runner metrics](docs/reference/runner-metrics.md) |
| **How-to** | [Embedding](docs/guides/embedding.md), [Pyodide integration](docs/guides/pyodide-integration.md), [Adding a command](docs/guides/adding-commands.md), [Troubleshooting](docs/guides/troubleshooting.md) |
| **Reference** | [Shell syntax](docs/reference/shell-syntax.md), [builtins](docs/reference/builtins.md), [utilities](docs/reference/utilities.md), [protocol](docs/reference/protocol.md), [sandbox & capabilities](docs/reference/sandbox-and-capabilities.md), [supported features](SUPPORTED.md) |
| **Explanation** | [Architecture](docs/explanation/architecture.md), [design decisions](docs/explanation/design-decisions.md), [ADRs](docs/adr/) |

## Acknowledgements

The Pyodide integration would not be possible without the outstanding work of the [Pyodide](https://pyodide.org/) team. They brought CPython to WebAssembly and built an ecosystem that makes running Python in the browser practical and reliable. wasmsh links directly into their Emscripten module, sharing the interpreter and filesystem — a testament to how well-designed their architecture is. Thank you to everyone who contributes to Pyodide.

## License

[Apache-2.0](LICENSE)
