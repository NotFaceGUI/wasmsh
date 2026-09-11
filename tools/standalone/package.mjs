import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  copyFileSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { join, relative, resolve } from "node:path";
import { gzipSync } from "node:zlib";

const [sourceArg, outputArg] = process.argv.slice(2);
if (!sourceArg || !outputArg) {
  throw new Error("usage: package.mjs <target-package-root> <output-directory>");
}

const repoRoot = resolve(import.meta.dirname, "../..");
const sourceRoot = resolve(sourceArg);
const outputRoot = resolve(outputArg);
const cargoToml = readFileSync(join(repoRoot, "Cargo.toml"), "utf8");
const version = cargoToml.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
if (!version) throw new Error("could not read workspace version from Cargo.toml");
const toolVersions = readFileSync(join(repoRoot, "tools/standalone/versions.env"), "utf8");

// A release tag must match the packaged version, otherwise the uploaded asset
// name and VERSION file silently disagree with the tag/release. A pre-release
// suffix (e.g. `v0.8.0-rc1`) is allowed; the base version must still match.
const refName = process.env.GITHUB_REF_NAME || "";
if (/^v\d/.test(refName) && refName !== `v${version}` && !refName.startsWith(`v${version}-`)) {
  throw new Error(
    `release tag ${refName} does not match package version v${version}`,
  );
}

const commit = process.env.GITHUB_SHA || execFileSync("git", ["rev-parse", "HEAD"], { cwd: repoRoot, encoding: "utf8" }).trim();
const shortCommit = commit.slice(0, 12);
const artifactName = `wasmsh-standalone-${version}-${shortCommit}`;
const artifactRoot = join(outputRoot, artifactName);
const archivePath = join(outputRoot, `${artifactName}.tar.gz`);

rmSync(artifactRoot, { recursive: true, force: true });
rmSync(archivePath, { force: true });
mkdirSync(artifactRoot, { recursive: true });

for (const target of ["web", "nodejs", "bundler"]) {
  copyTree(join(sourceRoot, target), join(artifactRoot, target));
}
mkdirSync(join(artifactRoot, "host"), { recursive: true });
copyFileSync(
  join(repoRoot, "tools/standalone/node-external-host.mjs"),
  join(artifactRoot, "host/node-external-host.mjs"),
);
copyFileSync(
  join(repoRoot, "tools/standalone/node-external-host.d.ts"),
  join(artifactRoot, "host/node-external-host.d.ts"),
);
copyFileSync(
  join(repoRoot, "tools/standalone/node-network-host.mjs"),
  join(artifactRoot, "host/node-network-host.mjs"),
);
copyFileSync(
  join(repoRoot, "tools/standalone/node-network-host.d.ts"),
  join(artifactRoot, "host/node-network-host.d.ts"),
);
for (const file of ["LICENSE", "SUPPORTED.md"]) {
  copyFileSync(join(repoRoot, file), join(artifactRoot, file));
}
copyFileSync(join(repoRoot, "tools/standalone/README.md"), join(artifactRoot, "README.md"));
writeFileSync(join(artifactRoot, "VERSION"), `${version}\n`);

function commandVersion(command, args) {
  try {
    return execFileSync(command, args, { encoding: "utf8" }).trim();
  } catch {
    return "unavailable";
  }
}

function pinnedVersion(name) {
  return toolVersions.match(new RegExp(`^${name}=([^\\r\\n]+)$`, "m"))?.[1] || "unknown";
}

function packageFiles(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? packageFiles(path) : [path];
  });
}

const targetManifest = {};
for (const target of ["web", "nodejs", "bundler"]) {
  const files = packageFiles(join(artifactRoot, target));
  const wasm = files.find((file) => file.endsWith(".wasm"));
  const bytes = readFileSync(wasm);
  targetManifest[target] = {
    wasm: relative(artifactRoot, wasm).replaceAll("\\", "/"),
    bytes: bytes.length,
    gzip_bytes: gzipSync(bytes, { level: 9 }).length,
    sha256: sha256(wasm),
  };
}

writeFileSync(
  join(artifactRoot, "build-manifest.json"),
  `${JSON.stringify(
    {
      schema_version: 1,
      artifact: artifactName,
      version,
      commit,
      ref: process.env.GITHUB_REF_NAME || "local",
      run_id: process.env.GITHUB_RUN_ID || null,
      target: "wasm32-unknown-unknown",
      features: ["default", "browser-core"],
      tools: {
        rustc: commandVersion("rustc", ["--version"]),
        wasm_pack: commandVersion(process.env.WASM_PACK_BIN || "wasm-pack", ["--version"]),
        wasm_bindgen_cli: commandVersion(process.env.WASM_BINDGEN_BIN || "wasm-bindgen", ["--version"]),
        wasm_opt: commandVersion(process.env.WASM_OPT_BIN || "wasm-opt", ["--version"]),
        binaryen_version: pinnedVersion("BINARYEN_VERSION"),
        node: process.version,
        npm: commandVersion("npm", ["--version"]),
      },
      targets: targetManifest,
      python_runtime_included: false,
      pyodide_runtime_included: false,
    },
    null,
    2,
  )}\n`,
);

function sha256(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

const checksumFiles = packageFiles(artifactRoot)
  .filter((file) => file !== join(artifactRoot, "SHA256SUMS"))
  .sort();
writeFileSync(
  join(artifactRoot, "SHA256SUMS"),
  checksumFiles
    .map((file) => `${sha256(file)}  ${relative(artifactRoot, file).replaceAll("\\", "/")}`)
    .join("\n") + "\n",
);

mkdirSync(outputRoot, { recursive: true });
execFileSync("tar", ["-czf", archivePath, "-C", outputRoot, artifactName], {
  cwd: repoRoot,
  stdio: "inherit",
});
console.log(`created ${archivePath}`);

function copyTree(source, destination) {
  mkdirSync(destination, { recursive: true });
  for (const entry of readdirSync(source, { withFileTypes: true })) {
    const from = join(source, entry.name);
    const to = join(destination, entry.name);
    if (entry.isDirectory()) copyTree(from, to);
    else copyFileSync(from, to);
  }
}
