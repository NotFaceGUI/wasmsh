import assert from "node:assert/strict";
import { get } from "node:http";
import { spawn } from "node:child_process";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

// A standalone package must not fetch Python/Pyodide assets during startup.
// The explicit network broker below uses node:http/node:https instead.
globalThis.fetch = () => {
  throw new Error("unexpected startup fetch in standalone package");
};

const packageRoot = resolve(process.argv[2] || "dist/standalone-pkg");
const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const moduleUrl = pathToFileURL(join(packageRoot, "nodejs", "wasmsh_browser.js"));
const { WasmShell } = await import(moduleUrl.href);
const hostModuleUrl = pathToFileURL(join(packageRoot, "host", "node-external-host.mjs"));
const {
  createNodeExternalExecutor,
  createNodeExternalStreamExecutor,
} = await import(hostModuleUrl.href);
const { installNodeNetworkBroker } = await import(
  pathToFileURL(join(packageRoot, "host", "node-network-host.mjs")).href,
);

function events(json) {
  return JSON.parse(json);
}

function output(reply, kind) {
  return reply
    .filter((event) => Object.hasOwn(event, kind))
    .flatMap((event) => event[kind])
    .map((byte) => String.fromCharCode(byte))
    .join("");
}

function exitCode(reply) {
  return reply.find((event) => Object.hasOwn(event, "Exit"))?.Exit;
}

function bytes(reply, kind) {
  return reply
    .filter((event) => Object.hasOwn(event, kind))
    .flatMap((event) => event[kind]);
}

function startNetworkFixture() {
  const fixture = join(repoRoot, "tools/standalone/network-fixture.mjs");
  const child = spawn(process.execPath, [fixture], {
    stdio: ["ignore", "pipe", "inherit"],
    windowsHide: true,
    shell: false,
  });
  return new Promise((resolveFixture, rejectFixture) => {
    let pending = "";
    const onData = (chunk) => {
      pending += chunk.toString();
      const newline = pending.indexOf("\n");
      if (newline < 0) return;
      child.stdout.off("data", onData);
      try {
        resolveFixture({ child, ...JSON.parse(pending.slice(0, newline)) });
      } catch (error) {
        rejectFixture(error);
      }
    };
    child.stdout.on("data", onData);
    child.once("error", rejectFixture);
    child.once("exit", (code) => {
      if (code !== null) rejectFixture(new Error(`network fixture exited with ${code}`));
    });
  });
}

function readNetworkCounts(port) {
  return new Promise((resolveCounts, rejectCounts) => {
    const request = get({ hostname: "127.0.0.1", port, path: "/__counts" }, (response) => {
      const chunks = [];
      response.on("data", (chunk) => chunks.push(chunk));
      response.on("end", () => {
        try {
          resolveCounts(JSON.parse(Buffer.concat(chunks).toString("utf8")));
        } catch (error) {
          rejectCounts(error);
        }
      });
    });
    request.on("error", rejectCounts);
  });
}

function stopNetworkFixture(child) {
  return new Promise((resolveStop) => {
    child.once("exit", resolveStop);
    child.kill();
  });
}

// This uses the same nodejs WASM package that the rest of this smoke loads.
// Rust performs policy and redirect validation; the host callback performs
// one real no-redirect HTTP request per Rust-approved hop.
const networkFixture = await startNetworkFixture();
const networkPort = networkFixture.port;
const networkUrl = `http://127.0.0.1:${networkPort}`;
const removeNetworkBroker = installNodeNetworkBroker();
const networkShell = new WasmShell();
networkShell.set_trusted_network_broker(true);
try {
  let networkInit = events(networkShell.init(0n, JSON.stringify({
    enabled: true,
    default_action: "deny",
    allow: [`127.0.0.1:${networkPort}`],
    deny: [],
  })));
  assert.ok(networkInit.some((event) => Object.hasOwn(event, "Version")));
  const networkReply = events(networkShell.exec(`curl -sL ${networkUrl}/redirect`));
  assert.equal(output(networkReply, "Stdout"), "network-ok");
  assert.equal(exitCode(networkReply), 0);
  assert.deepEqual(await readNetworkCounts(networkPort), {
    networkRequests: 2,
    crossOriginRequests: 0,
  });

  networkInit = events(networkShell.init(0n, JSON.stringify({
    enabled: true,
    default_action: "deny",
    allow: [`127.0.0.1:${networkPort}`],
    deny: [],
  })));
  assert.ok(networkInit.some((event) => Object.hasOwn(event, "Version")));
  const crossReply = events(networkShell.exec(`curl -sL ${networkUrl}/cross-redirect`));
  assert.notEqual(exitCode(crossReply), 0);
  assert.deepEqual(await readNetworkCounts(networkPort), {
    networkRequests: 3,
    crossOriginRequests: 0,
  });

  const oversizedReply = events(networkShell.exec(`curl --max-filesize 32 ${networkUrl}/large`));
  assert.equal(exitCode(oversizedReply), 63);
  assert.match(output(oversizedReply, "Stderr"), /response exceeds/);

  // Regression: an invalid rule must fail closed. Even with a trusted broker
  // installed, a rejected config must not leave an allow-all transport that
  // reaches the network.
  const invalidInit = events(networkShell.init(0n, JSON.stringify({
    enabled: true,
    default_action: "deny",
    allow: ["api.*.example.com"],
    deny: [],
  })));
  assert.ok(invalidInit.some(
    (event) => event.Diagnostic?.[0] === "Error"
      && /invalid network policy/.test(event.Diagnostic[1]),
  ));
  const invalidReply = events(networkShell.exec(`curl ${networkUrl}/ok`));
  assert.notEqual(exitCode(invalidReply), 0);
  assert.deepEqual(await readNetworkCounts(networkPort), {
    networkRequests: 4,
    crossOriginRequests: 0,
  });

  networkInit = events(networkShell.init(0n, "[]"));
  assert.ok(networkInit.some((event) => Object.hasOwn(event, "Version")));
  const deniedReply = events(networkShell.exec(`curl ${networkUrl}/ok`));
  assert.notEqual(exitCode(deniedReply), 0);
  assert.deepEqual(await readNetworkCounts(networkPort), {
    networkRequests: 4,
    crossOriginRequests: 0,
  });
} finally {
  removeNetworkBroker();
  await stopNetworkFixture(networkFixture.child);
}

const clockShell = new WasmShell();
const clockSamples = [1767225599500, 1767225600000];
clockShell.set_clock_callback(() => clockSamples.shift() ?? 1767225600000);
assert.ok(events(clockShell.init(0n, "[]")).some((event) => Object.hasOwn(event, "Version")));
let clockReply = events(clockShell.exec("date '+%Y-%m-%d %H:%M:%S %s'; date '+%Y-%m-%d %H:%M:%S %s'"));
assert.equal(output(clockReply, "Stdout"), "2025-12-31 23:59:59 1767225599\n2026-01-01 00:00:00 1767225600\n");
assert.equal(exitCode(clockReply), 0);
clockShell.clear_clock_callback();
clockReply = events(clockShell.exec("date +%s"));
assert.notEqual(exitCode(clockReply), 0);
assert.match(output(clockReply, "Stderr"), /clock unavailable|clock callback/);

const shell = new WasmShell();
shell.set_external_executor(createNodeExternalExecutor());
const fixture = join(repoRoot, "e2e/standalone/native/external-fixture.mjs");
const externalOptions = (mode, extra = {}) => JSON.stringify({
  cwd: process.cwd(),
  env: { TEST_ONLY: "standalone-smoke" },
  argv_prefix: [fixture, mode],
  max_input_bytes: 1024 * 1024,
  max_output_bytes: 1024 * 1024,
  timeout_ms: 1000,
  ...extra,
});
for (const [name, mode] of [
  ["hostcat", "cat"],
  ["hostemit", "emit"],
  ["hoststatus", "status"],
  ["hostargs", "args"],
]) {
  shell.register_external(name, process.execPath, externalOptions(mode));
}
shell.register_external("hosttimeout", process.execPath, externalOptions("sleep", { timeout_ms: 20 }));
shell.register_external("hostlimit", process.execPath, externalOptions("spam", { max_output_bytes: 32 }));
shell.register_external("hoststartfail", join(repoRoot, "missing-executable"), externalOptions("cat"));
const initEvents = events(shell.init(0n, "[]"));
assert.ok(initEvents.some((event) => Object.hasOwn(event, "Version")));
assert.deepEqual(JSON.parse(shell.external_commands()), [
  "hostcat",
  "hostemit",
  "hoststatus",
  "hostargs",
  "hosttimeout",
  "hostlimit",
  "hoststartfail",
]);

let reply = events(shell.exec("echo hello"));
assert.equal(output(reply, "Stdout"), "hello\n");
assert.equal(exitCode(reply), 0);

reply = events(shell.exec("printf persisted > /state.txt; echo $PWD"));
assert.equal(exitCode(reply), 0);
reply = events(shell.exec("cat /state.txt"));
assert.equal(output(reply, "Stdout"), "persisted");
assert.equal(exitCode(reply), 0);

reply = events(shell.write_file("/binary", new Uint8Array([0, 1, 255])));
assert.ok(reply.some((event) => Object.hasOwn(event, "FsChanged")));
reply = events(shell.read_file("/binary"));
assert.deepEqual(output(reply, "Stdout").split("").map((char) => char.charCodeAt(0)), [0, 1, 255]);

reply = events(shell.exec("exit 7"));
assert.equal(exitCode(reply), 7);

reply = events(shell.exec("printf hi | hostcat | wc -c"));
assert.equal(output(reply, "Stdout"), "2\n");
assert.equal(exitCode(reply), 0);

reply = events(shell.exec("hostcat <<'EOF'\nline one\nline two\nEOF"));
assert.equal(output(reply, "Stdout"), "line one\nline two\n");

reply = events(shell.exec("hostcat < /binary"));
assert.deepEqual(bytes(reply, "Stdout"), [0, 1, 255]);

reply = events(shell.exec("hostemit 2>&1 > /first.txt; cat /first.txt"));
assert.equal(output(reply, "Stdout"), "ERR\nOUT\n");
reply = events(shell.exec("hostemit > /second.txt 2>&1; cat /second.txt"));
assert.equal(output(reply, "Stdout"), "OUT\nERR\n");

reply = events(shell.exec("hoststatus 7 | hostcat; echo ${PIPESTATUS[0]} ${PIPESTATUS[1]}"));
assert.equal(output(reply, "Stdout"), "7 0\n");
reply = events(shell.exec("set -o pipefail; hoststatus 7 | hostcat"));
assert.equal(exitCode(reply), 7);

reply = events(shell.exec("hostargs 'arg with spaces' 'x;y' '$HOME'"));
assert.deepEqual(JSON.parse(output(reply, "Stdout")), ["arg with spaces", "x;y", "$HOME"]);

reply = events(shell.exec("hosttimeout"));
assert.equal(exitCode(reply), 124);
reply = events(shell.exec("hostlimit"));
assert.equal(exitCode(reply), 125);
reply = events(shell.exec("hoststartfail"));
assert.equal(exitCode(reply), 126);

const streamShell = new WasmShell();
streamShell.set_external_stream_executor(createNodeExternalStreamExecutor());
const streamOptions = (mode, extra = {}) => JSON.stringify({
  cwd: process.cwd(),
  env: { TEST_ONLY: "standalone-stream-smoke" },
  argv_prefix: [fixture, mode],
  max_input_bytes: 64 * 1024,
  max_output_bytes: 64 * 1024,
  stream_queue_bytes: 4096,
  stream_chunk_bytes: 1024,
  timeout_ms: 1000,
  ...extra,
});
for (const [name, mode] of [
  ["hostcat", "cat"],
  ["hostproducer", "producer"],
  ["hostdual", "dual"],
]) {
  streamShell.register_external(name, process.execPath, streamOptions(mode));
}
streamShell.register_external(
  "hosttimeoutstream",
  process.execPath,
  streamOptions("sleep", { timeout_ms: 20 }),
);
streamShell.register_external(
  "hoststartfailstream",
  join(repoRoot, "missing-executable"),
  streamOptions("cat"),
);

async function progressive(shellInstance, input) {
  const initial = events(shellInstance.start_run(input));
  assert.deepEqual(initial, ["Yielded"]);
  const replyEvents = [];
  for (let attempt = 0; attempt < 2000; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 1));
    const batch = events(shellInstance.poll_run());
    replyEvents.push(...batch.filter((event) => !Object.hasOwn(event, "Yielded")));
    if (batch.some((event) => Object.hasOwn(event, "Exit"))) {
      return replyEvents;
    }
  }
  throw new Error(`progressive run did not finish: ${input}`);
}

events(streamShell.init(0n, "[]"));
reply = await progressive(streamShell, "hostproducer | head -c 1");
assert.equal(output(reply, "Stdout"), "P");
assert.equal(exitCode(reply), 0);

reply = await progressive(streamShell, "yes | hostcat | head -n 1");
assert.equal(output(reply, "Stdout"), "y\n");
assert.equal(exitCode(reply), 0);

const largeInput = "x".repeat(8192);
reply = await progressive(streamShell, `printf '%s' '${largeInput}' | hostcat | wc -c`);
assert.equal(output(reply, "Stdout"), "8192\n");
assert.equal(exitCode(reply), 0);

reply = await progressive(streamShell, "hostdual");
assert.equal(bytes(reply, "Stdout").length, 32 * 1024);
assert.equal(bytes(reply, "Stderr").length, 32 * 1024);
reply = await progressive(streamShell, "hostdual | head -c 1");
assert.equal(output(reply, "Stdout"), "S");
assert.equal(exitCode(reply), 0);

reply = await progressive(streamShell, "hostdual |& hostcat");
assert.equal(bytes(reply, "Stdout").length, 64 * 1024);
assert.equal(bytes(reply, "Stderr").length, 0);
assert.equal(exitCode(reply), 0);

reply = await progressive(streamShell, "hosttimeoutstream");
assert.equal(exitCode(reply), 124);
reply = await progressive(streamShell, "hoststartfailstream");
assert.equal(exitCode(reply), 126);

events(streamShell.start_run("hostproducer | head -c 1000000"));
await new Promise((resolve) => setTimeout(resolve, 5));
events(streamShell.cancel());
reply = events(streamShell.poll_run());
assert.equal(exitCode(reply), 130);
reply = await progressive(streamShell, "echo recovered");
assert.equal(output(reply, "Stdout"), "recovered\n");
assert.equal(exitCode(reply), 0);

const isolated = new WasmShell();
events(isolated.init(0n, "[]"));
reply = events(isolated.exec("cat /state.txt"));
assert.notEqual(exitCode(reply), 0);

console.log("node smoke passed: execution, VFS persistence, binary data, exit status, isolation");
