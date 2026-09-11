/**
 * Network allowlist security tests — standalone browser (wasm-bindgen).
 *
 * Verifies the standalone browser's network boundary. The fixture uses
 * synchronous XHR, which is not a trusted redirect-aware broker, so even a
 * policy-allowed target must be refused before XHR is called.
 *
 * The local fixture URL is used for broker-refusal tests so CORS failures
 * cannot be mistaken for policy enforcement. The controlled-server Rust
 * integration test verifies actual per-hop blocking.
 */
import { test, expect } from "@playwright/test";

const FIXTURE_HOST = "localhost:3100";
const FIXTURE_URL = `http://${FIXTURE_HOST}/cors-echo.html`;

function decodeBytes(arr: number[]): string {
  return new TextDecoder().decode(new Uint8Array(arr));
}

function findStdout(events: any[]): string {
  const parts: number[] = [];
  for (const e of events) {
    if (e && typeof e === "object" && "Stdout" in e) parts.push(...e.Stdout);
  }
  return decodeBytes(parts);
}

function findStderr(events: any[]): string {
  const parts: number[] = [];
  for (const e of events) {
    if (e && typeof e === "object" && "Stderr" in e) parts.push(...e.Stderr);
  }
  return decodeBytes(parts);
}

function findExitCode(events: any[]): number | null {
  for (const e of events) {
    if (e && typeof e === "object" && "Exit" in e) return e.Exit;
  }
  return null;
}

async function initAndRun(
  page: any,
  allowedHosts: string[],
  command: string,
): Promise<{ stdout: string; stderr: string; exitCode: number | null }> {
  return page.evaluate(
    async ({
      hosts,
      cmd,
    }: {
      hosts: string[];
      cmd: string;
    }) => {
      const worker = (window as any).createShellWorker();

      function send(msg: any): Promise<any[]> {
        return new Promise((resolve, reject) => {
          const timeout = setTimeout(
            () => reject(new Error("worker timeout")),
            30_000,
          );
          worker.onmessage = (e: MessageEvent) => {
            clearTimeout(timeout);
            if (e.data.error) reject(new Error(e.data.error));
            else resolve(e.data.events);
          };
          worker.onerror = (e: ErrorEvent) => {
            clearTimeout(timeout);
            reject(new Error(e.message));
          };
          worker.postMessage(msg);
        });
      }

      await send({ type: "Init", step_budget: 0, allowed_hosts: hosts });
      const events = await send({ type: "Run", input: cmd });
      worker.terminate();

      // Extract results in browser context
      const stdoutParts: number[] = [];
      const stderrParts: number[] = [];
      let exitCode: number | null = null;
      for (const e of events) {
        if (e && typeof e === "object") {
          if ("Stdout" in e) stdoutParts.push(...e.Stdout);
          if ("Stderr" in e) stderrParts.push(...e.Stderr);
          if ("Exit" in e) exitCode = e.Exit;
        }
      }
      return {
        stdout: new TextDecoder().decode(new Uint8Array(stdoutParts)),
        stderr: new TextDecoder().decode(new Uint8Array(stderrParts)),
        exitCode,
      };
    },
    { hosts: allowedHosts, cmd: command },
  );
}

// ── Browser broker requirement ──────────────────────────────────

test("curl refuses an allowed host without a trusted broker", async ({ page }) => {
  await page.goto("/");
  const r = await initAndRun(
    page,
    [FIXTURE_HOST],
    `curl -sSL ${FIXTURE_URL}`,
  );
  expect(r.exitCode).not.toBe(0);
  expect(r.stdout).toBe("");
  expect(r.stderr).toContain("trusted redirect-aware broker");
});

test("wget refuses an allowed host without a trusted broker", async ({ page }) => {
  await page.goto("/");
  const r = await initAndRun(
    page,
    [FIXTURE_HOST],
    `wget -qO - ${FIXTURE_URL}`,
  );
  expect(r.exitCode).not.toBe(0);
  expect(r.stdout).toBe("");
  expect(r.stderr).toContain("trusted redirect-aware broker");
});

// ── Denied host ─────────────────────────────────────────────────
//
// The denied-host tests use fake external hostnames (example.com etc.).
// No actual fetch is attempted because the allowlist check fails first,
// so the tests do not depend on any external network being reachable.

test("curl to denied host (example.com) is blocked", async ({ page }) => {
  await page.goto("/");
  const r = await initAndRun(
    page,
    [FIXTURE_HOST],
    "curl https://example.com",
  );
  expect(r.exitCode).not.toBe(0);
  expect(r.stderr).toContain("denied");
});

test("wget to denied host (example.com) is blocked", async ({ page }) => {
  await page.goto("/");
  const r = await initAndRun(
    page,
    [FIXTURE_HOST],
    "wget -qO - https://example.com",
  );
  expect(r.exitCode).not.toBe(0);
  expect(r.stderr).toContain("denied");
});

// ── Subdomain not allowed with exact match ──────────────────────

test("curl to subdomain is blocked with exact host match", async ({
  page,
}) => {
  await page.goto("/");
  const r = await initAndRun(
    page,
    ["mayflower.de"],
    "curl https://evil.mayflower.de",
  );
  expect(r.exitCode).not.toBe(0);
});

// ── Similar hostnames blocked ───────────────────────────────────

test("curl to similar-looking hostnames is blocked", async ({ page }) => {
  await page.goto("/");
  for (const host of [
    "https://notmayflower.de",
    "https://mayflower.de.evil.com",
    "https://mayflower.com",
  ]) {
    const r = await initAndRun(page, ["mayflower.de"], `curl ${host}`);
    expect(r.exitCode).not.toBe(0);
  }
});

// ── Wildcard pattern ────────────────────────────────────────────

test("wildcard pattern blocks the apex (subdomains-only semantics)", async ({
  page,
}) => {
  await page.goto("/");
  // `*.localhost:3100` matches strict subdomains only; the apex
  // `localhost:3100` is NOT covered. Callers wanting the apex must list it
  // explicitly. See docs/reference/sandbox-and-capabilities.md.
  const r = await initAndRun(
    page,
    [`*.${FIXTURE_HOST}`],
    `curl ${FIXTURE_URL}`,
  );
  expect(r.exitCode).not.toBe(0);

  const r2 = await initAndRun(
    page,
    [`*.${FIXTURE_HOST}`],
    "curl https://example.com",
  );
  expect(r2.exitCode).not.toBe(0);
});

test("explicit apex + wildcard still require a trusted browser broker", async ({ page }) => {
  await page.goto("/");
  const r = await initAndRun(
    page,
    [FIXTURE_HOST, `*.${FIXTURE_HOST}`],
    `curl -sSL ${FIXTURE_URL}`,
  );
  expect(r.exitCode).not.toBe(0);
  expect(r.stderr).toContain("trusted redirect-aware broker");
});

// ── Empty allowlist ─────────────────────────────────────────────

test("empty allowlist blocks all hosts", async ({ page }) => {
  await page.goto("/");
  // An empty allowlist creates a backend that denies every host.
  // See `WasmShell::init` in crates/wasmsh-browser/src/lib.rs.
  const r = await initAndRun(page, [], `curl ${FIXTURE_URL}`);
  expect(r.exitCode).not.toBe(0);
  expect(r.stderr).toMatch(/denied|allowlist/);
});

test("structured network policy reaches the standalone WASM boundary", async ({
  page,
}) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    const init = await client.send({
      type: "Init",
      step_budget: 0,
      network_policy: {
        enabled: true,
        default_action: "allow",
        allow: [],
        deny: [],
      },
    });
    const run = await client.send({
      type: "Run",
      input: "curl http://localhost:3100/cors-echo.html",
    });
    client.close();
    return { init: init.events, run: run.events };
  });

  expect(result.init.some((entry: any) => "Version" in entry)).toBe(true);
  expect(findExitCode(result.run)).not.toBe(0);
  expect(findStderr(result.run)).toContain("trusted redirect-aware broker");
});

// ── curl | wc pipeline ──────────────────────────────────────────

test("curl pipeline cannot bypass the browser broker requirement", async ({ page }) => {
  await page.goto("/");
  const r = await initAndRun(
    page,
    [FIXTURE_HOST],
    `set -o pipefail; curl -sSL ${FIXTURE_URL} | wc -l`,
  );
  expect(r.exitCode).not.toBe(0);
  expect(r.stdout).toBe("0\n");
  expect(r.stderr).toContain("trusted redirect-aware broker");
});
