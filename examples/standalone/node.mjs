import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

const packageRoot = resolve(
  process.argv[2] || process.env.WASMSH_STANDALONE_ROOT || "dist/standalone",
);
const { WasmShell } = await import(
  pathToFileURL(join(packageRoot, "nodejs", "wasmsh_browser.js")).href,
);
const { createNodeExternalExecutor } = await import(
  pathToFileURL(join(packageRoot, "host", "node-external-host.mjs")).href,
);

function events(json) {
  return JSON.parse(json);
}

function text(reply, kind = "Stdout") {
  return new TextDecoder().decode(
    Uint8Array.from(reply.filter((event) => kind in event).flatMap((event) => event[kind])),
  );
}

function exitCode(reply) {
  return reply.find((event) => "Exit" in event)?.Exit;
}

const shell = new WasmShell();
shell.set_clock_callback(() => Date.now());
shell.set_external_executor(createNodeExternalExecutor());

// The executable and argv prefix are trusted host configuration. The shell
// can only supply parsed arguments after the registered command name.
const catScript = "process.stdin.on('data', chunk => process.stdout.write(chunk));";
shell.register_external(
  "hostcat",
  process.execPath,
  JSON.stringify({
    cwd: process.cwd(),
    env: { WASMSH_EXAMPLE: "1" },
    argv_prefix: ["-e", catScript],
  }),
);

const init = events(shell.init(100_000n, JSON.stringify({
  enabled: false,
  default_action: "deny",
  allow: [],
  deny: [],
})));
if (!init.some((event) => "Version" in event)) {
  throw new Error(`wasmsh initialization failed: ${JSON.stringify(init)}`);
}

let reply = events(shell.exec("printf 'hello' | hostcat"));
if (text(reply) !== "hello" || exitCode(reply) !== 0) {
  throw new Error(`external example failed: ${JSON.stringify(reply)}`);
}
reply = events(shell.exec("date -u '+%F %T'; printf '\\0\\xff' > /binary; cat /binary"));
if (!text(reply).startsWith(`${new Date().toISOString().slice(0, 10)} `)) {
  throw new Error(`live clock example failed: ${JSON.stringify(reply)}`);
}
if (exitCode(reply) !== 0) {
  throw new Error(`binary example failed: ${JSON.stringify(reply)}`);
}

console.log("standalone Node example passed: live clock, VFS bytes, and external argv/stdin");
