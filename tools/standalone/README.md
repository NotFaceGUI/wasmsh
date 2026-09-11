# wasmsh standalone sh WASM

This archive contains the independently built `wasmsh-browser` shell for the
`wasm32-unknown-unknown` target. Use the loader and matching `.wasm` file from
one target directory together.

Each of `web/`, `nodejs/`, and `bundler/` contains the complete wasm-bindgen
loader, declarations, package metadata, and WASM binary for that target.

The archive root contains `VERSION`, `build-manifest.json`, `SHA256SUMS`,
`LICENSE`, and `SUPPORTED.md`. The checksum file covers every shipped file
except itself.

The current public binding is `WasmShell`: construct it, install the host
capabilities you intend to grant, call `init(0n, "[]")`, then call
`exec(command)`. Methods return JSON-encoded protocol events. `init` resets
the session; it is not a way to refresh the clock or capabilities in place.

The shell's wall clock is installed by the host with
`set_clock_callback(() => Date.now())`. The callback must synchronously return
a safe integer Unix timestamp in milliseconds. Tests and deterministic hosts
can use `set_fixed_time_ms(unix_ms)` or install a sequence callback. Call
`clear_clock_callback()` to make time-dependent commands fail explicitly.

Register finite external commands with
`register_external(name, fixedExecutable, optionsJson)` and inspect them with
`external_commands()`. Then install the synchronous host executor exported by
`host/node-external-host.mjs`:

```js
import { WasmShell } from "./nodejs/wasmsh_browser.js";
import { createNodeExternalExecutor } from "./host/node-external-host.mjs";

const shell = new WasmShell();
shell.set_external_executor(createNodeExternalExecutor());
shell.register_external("hostcat", "/trusted/bin/hostcat", JSON.stringify({
  cwd: "/trusted/work",
  env: { TOOL_MODE: "cat" },
  vfs_path_mappings: [{ vfs_prefix: "/workspace", host_prefix: "/trusted/workspace" }],
  max_input_bytes: 16 * 1024 * 1024,
  max_output_bytes: 16 * 1024 * 1024,
  timeout_ms: 30_000,
}));
```

For a runnable example, see `examples/standalone/node.mjs`. The matching API
reference is `docs/guides/standalone-embedding.md`.

The Node adapter uses the parsed shell argv once with `shell: false`, closes
stdin after the provided bytes, drains both output pipes, does not inherit the
host environment, and requires an explicit host `cwd`. Default finite limits
are 16 MiB combined input/output and 30 seconds; registration rejects values
above 64 MiB or 300 seconds. `124` means timeout, `125` means a finite I/O
limit was hit, `126` means the registered executable could not be started (or
the host lacks native-process support), and `127` remains “not registered”.
VFS paths are not host paths: only explicitly mapped absolute VFS prefixes are
translated, and other absolute VFS arguments fail closed.

The browser binding supports registration and capability queries, but without a
trusted executor it returns 126 with an explicit native-process diagnostic.
The finite interface buffers stdin and both output streams and has no early
output, backpressure, or cancellation guarantee. The progressive stream
protocol below is the separate interface for those semantics.

Progressive external execution is available in the standalone Node binding
through `set_external_stream_executor(callback)`, `start_run(input)`, and
`poll_run()`. The stream callback receives a synchronous request object whose
operation is `start`, `write_stdin`, `close_stdin`, `poll`, or `cancel`; `poll`
returns bounded stdout/stderr chunks, per-stream EOF flags, stdin writability,
and the eventual status. `host/node-external-host.mjs` implements this with
`spawn`, paused bounded queues, explicit drain handling, and process-tree kill.
Do not call `exec` for a stream-only host: `exec` intentionally retains the
finite synchronous compatibility path.

Node network access is an explicit second host capability. Install
`host/node-network-host.mjs` with `installNodeNetworkBroker()`, set
`shell.set_trusted_network_broker(true)`, and pass a structured policy to
`init`. The broker makes one standard `http`/`https` request per callback and
never follows redirects. Rust checks the policy and every redirect target
before the next request. The browser fixture intentionally refuses its
synchronous XHR path because it cannot provide the same pre-request
per-hop guarantee.

Capability tiers:

- Base standalone: in-process shell, POSIX VFS, binary protocol events,
  session reset/isolation, and cooperative step-budget cancellation.
- Full Node host integration: the base tier plus live clock, structured
  network broker, finite external commands, and progressive external pipes.
- Browser limitation: clock and VFS work, while native external processes and
  trusted redirect-aware networking require host services not supplied by a
  browser worker.

This standalone artifact contains no Python, Pyodide, CPython, pip, or
micropip runtime and does not download them during initialization. The build
verification checks file paths, package metadata, manifest sizes/hashes, and
the complete `SHA256SUMS` file; Node smoke also fails if startup calls
`fetch`.

The build uses the pinned Rust toolchain, `wasm-pack`, `wasm-bindgen`,
Binaryen/`wasm-opt`, Node.js, and locked Playwright dependencies. CI downloads
the exact Binaryen archive listed in `versions.env`, verifies its SHA-256, and
then runs `wasm-pack` with `--mode no-install`. A build fails if any required
tool is missing or reports a different pinned version; it cannot silently skip
optimization or replace a tool with a different version.
