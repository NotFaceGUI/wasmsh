# Standalone WASM Embedding

The standalone artifact contains three wasm-bindgen consumers of the same
`wasm32-unknown-unknown` WASM: `nodejs/`, `bundler/`, and `web/`. Always load
the JavaScript loader and `.wasm` file from the same target directory.

## Node

```js
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = "/path/to/extracted/wasmsh-standalone-0.8.0-<commit>";
const { WasmShell } = await import(
  pathToFileURL(join(root, "nodejs", "wasmsh_browser.js")).href,
);
const { createNodeExternalExecutor } = await import(
  pathToFileURL(join(root, "host", "node-external-host.mjs")).href,
);

const shell = new WasmShell();
shell.set_clock_callback(() => Date.now());
shell.set_external_executor(createNodeExternalExecutor());
shell.register_external("hostcat", process.execPath, JSON.stringify({
  cwd: process.cwd(),
  env: { TOOL_MODE: "cat" },
  argv_prefix: ["-e", "process.stdin.on('data', c => process.stdout.write(c))"],
}));

const init = JSON.parse(shell.init(100_000n, JSON.stringify({
  enabled: false,
  default_action: "deny",
  allow: [],
  deny: [],
})));
const result = JSON.parse(shell.exec("printf hello | hostcat"));
```

`init` resets the VFS, variables, functions, cwd, active run, and pending
processes. Keep one `WasmShell` per session and call `exec` for finite
commands. Use `start_run` followed by `poll_run` for streaming external
commands; each poll must be allowed to return to the Node event loop.

## Network

The WASM network import is synchronous. Node must install the shipped broker
before enabling the trusted flag:

```js
const { installNodeNetworkBroker } = await import(
  pathToFileURL(join(root, "host", "node-network-host.mjs")).href,
);
const removeBroker = installNodeNetworkBroker();
shell.set_trusted_network_broker(true);
const policy = {
  enabled: true,
  default_action: "deny",
  allow: ["api.example.com", "*.assets.example.com:443"],
  deny: ["blocked.api.example.com"],
};
const networkInit = JSON.parse(shell.init(100_000n, JSON.stringify(policy)));
```

The broker never follows `Location`; Rust checks the next URL against the
policy before invoking the broker again. A disabled policy or a denied host
fails before network I/O. Use the browser only with a separately audited
redirect-aware broker: the included synchronous XHR fixture is deliberately
refused because it cannot enforce this contract.

## API Contract

`WasmShell` methods return a JSON-encoded array of protocol events:

| Method | Contract |
| --- | --- |
| `init(stepBudget, networkConfigJson)` | Resets the session and returns `Version` or an initialization diagnostic. Accepts a legacy host array or structured policy. |
| `exec(input)` | Drains one finite run and ends with `Exit(code)`. |
| `start_run(input)` | Starts progressive execution and normally returns `Yielded`. |
| `poll_run()` | Returns output chunks and either `Yielded` or final `Exit(code)`. |
| `cancel()` | Requests cooperative cancellation; the active run finishes as `Exit(130)`. |
| `write_file(path, bytes)` / `read_file(path)` | Transfer raw VFS bytes; paths are POSIX virtual paths. |
| `register_external(name, executable, optionsJson)` | Adds a fixed executable mapping; the host executor receives parsed argv and byte stdin. |
| `set_external_stream_executor(callback)` | Installs the synchronous start/write/close/poll/cancel process bridge. |
| `set_clock_callback(callback)` | Installs a synchronous safe-integer Unix epoch millisecond provider. |

External `optionsJson` uses `cwd`, explicit `env`, `argv_prefix`, optional
`vfs_path_mappings`, `max_input_bytes`, `max_output_bytes`, `timeout_ms`,
`stream_queue_bytes`, and `stream_chunk_bytes`. The Node adapter uses
`shell: false`; `argv[0]` is the registered command name and is removed once
before spawn. Status `124` is timeout, `125` is a finite I/O limit, `126` is
host start/capability failure, and `127` is an unregistered command.

See [the protocol reference](../reference/protocol.md) for event ordering and
[the package README](../../tools/standalone/README.md) for artifact details.
