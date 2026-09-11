import assert from "node:assert/strict";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { createNodeExternalExecutor } from "./node-external-host.mjs";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const fixture = join(repoRoot, "e2e/standalone/native/external-fixture.mjs");
const executor = createNodeExternalExecutor();

function options(mode, overrides = {}) {
  return JSON.stringify({
    cwd: repoRoot,
    env: { TEST_ONLY: "explicit-env" },
    argv_prefix: [fixture, mode],
    max_input_bytes: 1024,
    max_output_bytes: 1024,
    timeout_ms: 1000,
    ...overrides,
  });
}

let result = executor(
  "hostcat",
  process.execPath,
  ["hostcat"],
  new Uint8Array([0, 1, 255]),
  options("cat"),
);
assert.equal(result.status, 0);
assert.deepEqual([...result.stdout], [0, 1, 255]);
assert.deepEqual([...result.stderr], []);

result = executor(
  "hostemit",
  process.execPath,
  ["hostemit"],
  new Uint8Array(),
  options("emit"),
);
assert.equal(result.status, 0);
assert.equal(result.stdout.toString(), "OUT\n");
assert.equal(result.stderr.toString(), "ERR\n");

result = executor(
  "hostargs",
  process.execPath,
  ["hostargs", "arg with spaces", "x;y", "$HOME"],
  new Uint8Array(),
  options("args"),
);
assert.equal(result.status, 0);
assert.deepEqual(JSON.parse(result.stdout.toString()), [
  "arg with spaces",
  "x;y",
  "$HOME",
]);

result = executor(
  "hostenv",
  process.execPath,
  ["hostenv"],
  new Uint8Array(),
  options("env"),
);
assert.equal(result.stdout.toString(), "explicit-env");

result = executor(
  "hostpath",
  process.execPath,
  ["hostpath", "/workspace/input.bin"],
  new Uint8Array(),
  options("args", {
    vfs_path_mappings: [{ vfs_prefix: "/workspace", host_prefix: repoRoot }],
  }),
);
assert.equal(result.status, 0);
assert.equal(JSON.parse(result.stdout.toString())[0], join(repoRoot, "input.bin"));

result = executor(
  "hoststatus",
  process.execPath,
  ["hoststatus", "7"],
  new Uint8Array(),
  options("status"),
);
assert.equal(result.status, 7);

result = executor(
  "hoststartfail",
  join(repoRoot, "missing-executable"),
  ["hoststartfail"],
  new Uint8Array(),
  options("cat"),
);
assert.equal(result.status, 126);
assert.match(result.stderr.toString(), /failed to start/);

result = executor(
  "hosttimeout",
  process.execPath,
  ["hosttimeout"],
  new Uint8Array(),
  options("sleep", { timeout_ms: 20 }),
);
assert.equal(result.status, 124);

result = executor(
  "hostlimit",
  process.execPath,
  ["hostlimit"],
  new Uint8Array(),
  options("spam", { max_output_bytes: 32 }),
);
assert.equal(result.status, 125);

result = executor(
  "hostunmapped",
  process.execPath,
  ["hostunmapped", "/not-mapped/file"],
  new Uint8Array(),
  options("args"),
);
assert.equal(result.status, 126);
assert.match(result.stderr.toString(), /unmapped VFS path/);

console.log("external smoke passed: argv, EOF/binary stdin, dual streams, env/cwd/path mapping, status, startup, timeout, and limits");

