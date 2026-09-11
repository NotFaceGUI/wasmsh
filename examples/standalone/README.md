# Standalone Node embedding

This example consumes an extracted standalone artifact. It does not build
WASM, download Python/Pyodide assets, or invoke a shell on the host.

Build and package the artifact first:

```sh
bash tools/standalone/build.sh dist/standalone-pkg
node tools/standalone/package.mjs dist/standalone-pkg dist/standalone
tar -xzf dist/standalone/wasmsh-standalone-*.tar.gz -C dist/standalone
node examples/standalone/node.mjs dist/standalone/wasmsh-standalone-<version>-<commit>
```

The example loads the `nodejs/` target, installs the host wall-clock
callback, registers one fixed non-shell external command, and checks binary
VFS data. The `bundler/` target uses the same `WasmShell` API. Browser workers
use the `web/` target and must provide their own worker bootstrap; native
processes are not available in a browser.

For network access, import `host/node-network-host.mjs`, call
`installNodeNetworkBroker()`, then call `shell.set_trusted_network_broker(true)`
before `init`. Pass a structured `network_policy` with the smallest required
allow/deny rules. The broker performs one synchronous HTTP(S) transport
request per call and never follows redirects; Rust validates each redirect
hop before the next callback. Do not enable the flag without installing a
broker that has the same guarantees.
