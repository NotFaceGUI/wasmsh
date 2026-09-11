import { createHash } from "node:crypto";
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join, relative } from "node:path";

const packageRoot = process.argv[2];
if (!packageRoot) {
  throw new Error("usage: verify-package.mjs <package-root>");
}

const targets = ["web", "nodejs", "bundler"];
const forbiddenPath = /pyodide|python_stdlib|cpython|micropip|\.whl/i;

function fail(message) {
  throw new Error(`standalone package verification failed: ${message}`);
}

function pinnedVersion(name) {
  const source = readFileSync(
    join(import.meta.dirname, "versions.env"),
    "utf8",
  );
  return source.match(new RegExp(`^${name}=([^\\r\\n]+)$`, "m"))?.[1] || "unknown";
}

function filesUnder(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? filesUnder(path) : [path];
  });
}

if (!existsSync(packageRoot)) fail(`missing package directory ${packageRoot}`);

for (const target of targets) {
  const directory = join(packageRoot, target);
  if (!existsSync(directory)) fail(`missing target directory ${target}`);

  const files = filesUnder(directory);
  const wasm = files.filter((file) => file.endsWith(".wasm"));
  const loaders = files.filter((file) => file.endsWith(".js"));
  const declarations = files.filter((file) => file.endsWith(".d.ts"));
  if (wasm.length !== 1) fail(`${target} must contain exactly one .wasm file`);
  if (loaders.length === 0) fail(`${target} has no JavaScript loader`);
  if (declarations.length === 0) fail(`${target} has no TypeScript declaration`);
  const metadataFile = files.find((file) => file.endsWith("package.json"));
  if (!metadataFile) {
    fail(`${target} has no package metadata`);
  }
  let metadata;
  try {
    metadata = JSON.parse(readFileSync(metadataFile, "utf8"));
  } catch (error) {
    fail(`${target} has invalid package metadata: ${error.message}`);
  }
  if (typeof metadata.version !== "string" || metadata.version.length === 0) {
    fail(`${target} package metadata has no version`);
  }

  const magic = readFileSync(wasm[0]).subarray(0, 4).toString("hex");
  if (magic !== "0061736d") fail(`${target} WASM has an invalid magic header`);
}

for (const file of filesUnder(packageRoot)) {
  const relativePath = relative(packageRoot, file);
  if (forbiddenPath.test(relativePath)) {
    fail(`forbidden Python/Pyodide asset path ${relativePath}`);
  }
  if (file.endsWith(".js")) {
    const source = readFileSync(file, "utf8");
    if (/https?:\/\//i.test(source)) {
      fail(`standalone JavaScript must not contain a remote startup URL: ${relativePath}`);
    }
  }
}

const manifestPath = join(packageRoot, "build-manifest.json");
if (existsSync(manifestPath)) {
  const requiredFiles = ["LICENSE", "README.md", "SUPPORTED.md", "VERSION", "SHA256SUMS"];
  for (const file of requiredFiles) {
    if (!existsSync(join(packageRoot, file))) fail(`final artifact is missing ${file}`);
  }
  for (const file of ["node-external-host.mjs", "node-external-host.d.ts"]) {
    if (!existsSync(join(packageRoot, "host", file))) {
      fail(`standalone artifact is missing host/${file}`);
    }
  }
  for (const file of ["node-network-host.mjs", "node-network-host.d.ts"]) {
    if (!existsSync(join(packageRoot, "host", file))) {
      fail(`standalone artifact is missing host/${file}`);
    }
  }

  let manifest;
  try {
    manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  } catch (error) {
    fail(`invalid build-manifest.json: ${error.message}`);
  }
  if (manifest.schema_version !== 1) fail("unsupported build manifest schema");
  if (manifest.target !== "wasm32-unknown-unknown") fail("unexpected build target");
  if (manifest.python_runtime_included !== false) fail("Python runtime is marked as included");
  if (manifest.pyodide_runtime_included !== false) fail("Pyodide runtime is marked as included");
  if (!manifest.tools?.wasm_opt || manifest.tools.wasm_opt === "unavailable") {
    fail("build manifest does not record a usable wasm-opt version");
  }
  // A package built with an unpinned/incompatible optimizer must not pass the
  // authoritative post-extraction verification.
  const expectedBinaryen = pinnedVersion("BINARYEN_VERSION");
  if (manifest.tools.binaryen_version !== expectedBinaryen) {
    fail(
      `build manifest Binaryen version ${manifest.tools.binaryen_version} does not match pinned ${expectedBinaryen}`,
    );
  }
  if (!manifest.tools.wasm_opt.includes(`version ${expectedBinaryen}`)) {
    fail(
      `build manifest wasm-opt string "${manifest.tools.wasm_opt}" does not report Binaryen ${expectedBinaryen}`,
    );
  }
  let sharedWasmSha256;
  for (const target of targets) {
    const wasmPath = manifest.targets?.[target]?.wasm;
    if (typeof wasmPath !== "string" || !existsSync(join(packageRoot, wasmPath))) {
      fail(`build manifest has no usable ${target} WASM entry`);
    }
    const wasmFile = join(packageRoot, wasmPath);
    const targetEntry = manifest.targets[target];
    const wasmBytes = readFileSync(wasmFile);
    if (targetEntry.bytes !== wasmBytes.length) {
      fail(`${target} manifest byte count does not match the shipped WASM`);
    }
    const wasmSha256 = createHash("sha256").update(wasmBytes).digest("hex");
    if (targetEntry.sha256 !== wasmSha256) {
      fail(`${target} manifest SHA256 does not match the shipped WASM`);
    }
    if (sharedWasmSha256 === undefined) sharedWasmSha256 = wasmSha256;
    else if (sharedWasmSha256 !== wasmSha256) fail("target WASM files are not byte-identical");
    if (typeof targetEntry.gzip_bytes !== "number" || targetEntry.gzip_bytes < 1) {
      fail(`${target} manifest has no gzip size`);
    }
    const metadataFile = filesUnder(join(packageRoot, target)).find((file) => file.endsWith("package.json"));
    const metadata = JSON.parse(readFileSync(metadataFile, "utf8"));
    if (metadata.version !== manifest.version) {
      fail(`${target} package version ${metadata.version} differs from manifest ${manifest.version}`);
    }
  }

  const version = readFileSync(join(packageRoot, "VERSION"), "utf8").trim();
  if (version !== manifest.version) fail("VERSION differs from build manifest");

  const checksumPath = join(packageRoot, "SHA256SUMS");
  const checksumLines = readFileSync(checksumPath, "utf8")
    .split(/\r?\n/)
    .filter(Boolean);
  const expectedFiles = filesUnder(packageRoot)
    .filter((file) => file !== checksumPath)
    .map((file) => relative(packageRoot, file).replaceAll("\\", "/"))
    .sort();
  const checksums = new Map();
  for (const line of checksumLines) {
    const match = line.match(/^([0-9a-f]{64})  (.+)$/);
    if (!match) fail(`invalid SHA256SUMS line: ${line}`);
    if (checksums.has(match[2])) fail(`duplicate SHA256SUMS entry: ${match[2]}`);
    checksums.set(match[2], match[1]);
  }
  const actualFiles = [...checksums.keys()].sort();
  if (JSON.stringify(actualFiles) !== JSON.stringify(expectedFiles)) {
    fail("SHA256SUMS does not cover exactly the shipped files");
  }
  for (const file of expectedFiles) {
    const digest = createHash("sha256").update(readFileSync(join(packageRoot, file))).digest("hex");
    if (checksums.get(file) !== digest) fail(`SHA256SUMS mismatch for ${file}`);
  }
}

console.log(`verified ${targets.length} targets without Python/Pyodide runtime assets`);
