import assert from "node:assert/strict";
import { copyFileSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const packageRoot = resolve(process.argv[2] || "dist/standalone-pkg");
const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const esbuildPath = join(repoRoot, "e2e/standalone/node_modules/esbuild/lib/main.js");
const { build } = await import(pathToFileURL(esbuildPath).href);
const scratch = mkdtempSync(join(tmpdir(), "wasmsh-bundler-"));
const bundlePath = join(scratch, "consumer.mjs");
const source = `import { WasmShell } from ${JSON.stringify(
  join(packageRoot, "bundler", "wasmsh_browser.js"),
)};\nexport { WasmShell };\n`;

try {
  await build({
    bundle: true,
    format: "esm",
    platform: "node",
    target: "node22",
    external: ["*.wasm", "wasmsh_browser_bg.js"],
    plugins: [{
      name: "preserve-wasm-bindgen-glue",
      setup(bundler) {
        bundler.onResolve({ filter: /wasmsh_browser_bg\.js$/ }, (args) => ({
          path: args.path,
          external: true,
        }));
      },
    }],
    stdin: {
      contents: source,
      resolveDir: repoRoot,
      sourcefile: "standalone-bundler-consumer.mjs",
    },
    outfile: bundlePath,
  });
  copyFileSync(
    join(packageRoot, "bundler", "wasmsh_browser_bg.wasm"),
    join(scratch, "wasmsh_browser_bg.wasm"),
  );
  copyFileSync(
    join(packageRoot, "bundler", "wasmsh_browser_bg.js"),
    join(scratch, "wasmsh_browser_bg.js"),
  );
  const { WasmShell } = await import(pathToFileURL(bundlePath).href);
  const shell = new WasmShell();
  const clockSamples = [1767225600000];
  shell.set_clock_callback(() => clockSamples.shift() ?? 1767225600000);
  const initEvents = JSON.parse(shell.init(0n, "[]"));
  assert.ok(initEvents.some((event) => Object.hasOwn(event, "Version")));
  const runEvents = JSON.parse(shell.exec("date -u '+%F %T'; printf bundler-ok"));
  const stdout = runEvents
    .filter((event) => Object.hasOwn(event, "Stdout"))
    .flatMap((event) => event.Stdout)
    .map((byte) => String.fromCharCode(byte))
    .join("");
  assert.equal(stdout, "2026-01-01 00:00:00\nbundler-ok");
  assert.equal(runEvents.find((event) => Object.hasOwn(event, "Exit"))?.Exit, 0);

  const binary = new Uint8Array([0, 1, 2, 127, 128, 255]);
  JSON.parse(shell.write_file("/bundler-binary", binary));
  const binaryReply = JSON.parse(shell.read_file("/bundler-binary"));
  assert.deepEqual(
    binaryReply
      .filter((event) => Object.hasOwn(event, "Stdout"))
      .flatMap((event) => event.Stdout),
    [...binary],
  );

  const denied = JSON.parse(
    shell.exec("printf secret > /private; chmod 000 /private; cat /private"),
  );
  assert.notEqual(denied.find((event) => Object.hasOwn(event, "Exit"))?.Exit, 0);

  const isolated = new WasmShell();
  JSON.parse(isolated.init(0n, "[]"));
  const isolatedRead = JSON.parse(isolated.read_file("/bundler-binary"));
  assert.ok(isolatedRead.some((event) => Object.hasOwn(event, "Diagnostic")));

  console.log("bundler smoke passed: import, clock, binary VFS, permissions, and isolation");
} finally {
  rmSync(scratch, { recursive: true, force: true });
}
