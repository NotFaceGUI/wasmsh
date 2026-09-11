/**
 * Network allowlist security tests — Pyodide browser (Emscripten).
 *
 * The browser worker uses synchronous XHR for the Rust network ABI. Since
 * that API cannot guarantee per-hop redirect checks, allowed targets must be
 * refused unless a trusted broker is wired in.
 *
 * The local fixture URL is used so CORS failures cannot be mistaken for
 * policy enforcement. Actual per-hop blocking is verified by the controlled
 * server integration test.
 *
 * Tests run through the Pyodide browser worker which uses Emscripten's
 * extern "C" FFI for network calls (wasmsh_js_http_fetch → sync XHR).
 */
import { test, expect } from "@playwright/test";

const FIXTURE_HOST = "localhost:3200";
const FIXTURE_URL = `http://${FIXTURE_HOST}/cors-echo.html`;

function decodeBytes(arr: number[]): string {
  return new TextDecoder().decode(new Uint8Array(arr));
}

async function send(page: any, msg: any): Promise<any[]> {
  return page.evaluate(async (m: any) => (window as any)._pySend(m), msg);
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

test.beforeEach(async ({ page }) => {
  await page.goto("/");
  await page.evaluate(() => (window as any)._pyWorkerReadyPromise);
});

async function initWithHosts(page: any, allowedHosts: string[]) {
  await send(page, {
    type: "Init",
    step_budget: 0,
    allowed_hosts: allowedHosts,
  });
}

async function run(page: any, command: string) {
  const events = await send(page, { type: "Run", input: command });
  return {
    stdout: findStdout(events),
    stderr: findStderr(events),
    exitCode: findExitCode(events),
  };
}

// ── Allowed host ────────────────────────────────────────────────

test("curl refuses an allowed host without a trusted broker", async ({ page }) => {
  await initWithHosts(page, [FIXTURE_HOST]);
  const r = await run(page, `curl -sSL ${FIXTURE_URL}`);

  expect(r.exitCode).not.toBe(0);
  expect(r.stdout).toBe("");
  expect(r.stderr).toContain("synchronous XHR is refused");
});

test("wget refuses an allowed host without a trusted broker", async ({ page }) => {
  await initWithHosts(page, [FIXTURE_HOST]);
  const r = await run(page, `wget -qO - ${FIXTURE_URL}`);

  expect(r.exitCode).not.toBe(0);
  expect(r.stdout).toBe("");
  expect(r.stderr).toContain("synchronous XHR is refused");
});

// ── Denied host ─────────────────────────────────────────────────
//
// The denied-host tests use fake external hostnames (example.com etc.).
// No actual fetch is attempted because the allowlist check fails first,
// so the tests do not depend on any external network being reachable.

test("curl to denied host (example.com) is blocked", async ({ page }) => {
  await initWithHosts(page, [FIXTURE_HOST]);
  const r = await run(page, "curl https://example.com");

  expect(r.exitCode).not.toBe(0);
  expect(r.stderr).toContain("denied");
});

test("wget to denied host (example.com) is blocked", async ({ page }) => {
  await initWithHosts(page, [FIXTURE_HOST]);
  const r = await run(page, "wget -qO - https://example.com");

  expect(r.exitCode).not.toBe(0);
  expect(r.stderr).toContain("denied");
});

// ── Subdomain blocked with exact match ──────────────────────────

test("curl to subdomain is blocked with exact host match", async ({
  page,
}) => {
  await initWithHosts(page, ["mayflower.de"]);
  const r = await run(page, "curl https://evil.mayflower.de");

  expect(r.exitCode).not.toBe(0);
});

// ── Similar hostnames blocked ───────────────────────────────────

test("curl to similar-looking hostnames is blocked", async ({ page }) => {
  await initWithHosts(page, ["mayflower.de"]);
  for (const host of [
    "https://notmayflower.de",
    "https://mayflower.de.evil.com",
    "https://mayflower.com",
  ]) {
    const r = await run(page, `curl ${host}`);
    expect(r.exitCode).not.toBe(0);
  }
});

// ── Wildcard pattern ────────────────────────────────────────────

test("wildcard pattern blocks the apex (subdomains-only semantics)", async ({
  page,
}) => {
  // `*.localhost:3200` matches strict subdomains only; the apex
  // `localhost:3200` is NOT covered. Callers wanting the apex must list it
  // explicitly. See docs/reference/sandbox-and-capabilities.md.
  await initWithHosts(page, [`*.${FIXTURE_HOST}`]);
  const r = await run(page, `curl ${FIXTURE_URL}`);
  expect(r.exitCode).not.toBe(0);

  const r2 = await run(page, "curl https://example.com");
  expect(r2.exitCode).not.toBe(0);
});

test("explicit apex + wildcard still require a trusted browser broker", async ({ page }) => {
  await initWithHosts(page, [FIXTURE_HOST, `*.${FIXTURE_HOST}`]);
  const r = await run(page, `curl -sSL ${FIXTURE_URL}`);
  expect(r.exitCode).not.toBe(0);
  expect(r.stderr).toContain("synchronous XHR is refused");
});

// ── Empty allowlist ─────────────────────────────────────────────

test("empty allowlist blocks all hosts", async ({ page }) => {
  // An empty allowlist creates a backend that denies every host.
  // See `wasmsh_runtime_command` in crates/wasmsh-pyodide/src/lib.rs.
  await initWithHosts(page, []);
  const r = await run(page, `curl ${FIXTURE_URL}`);

  expect(r.exitCode).not.toBe(0);
  expect(r.stderr).toMatch(/denied|allowlist/);
});

// ── curl piped to wc ────────────────────────────────────────────

test("curl pipeline cannot bypass the browser broker requirement", async ({ page }) => {
  await initWithHosts(page, [FIXTURE_HOST]);
  const r = await run(page, `set -o pipefail; curl -sSL ${FIXTURE_URL} | wc -l`);

  expect(r.exitCode).not.toBe(0);
  expect(r.stdout).toBe("");
  expect(r.stderr).toContain("synchronous XHR is refused");
});
