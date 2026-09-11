import { test, expect } from "@playwright/test";

function stdout(events: any[]): string {
  const event = events.find((entry) => "Stdout" in entry);
  return event ? new TextDecoder().decode(new Uint8Array(event.Stdout)) : "";
}

function stderr(events: any[]): string {
  const event = events.find((entry) => "Stderr" in entry);
  return event ? new TextDecoder().decode(new Uint8Array(event.Stderr)) : "";
}

function hasExit(events: any[], status: number): boolean {
  return events.some((entry) => "Exit" in entry && entry.Exit === status);
}

test("uses a fresh host wall-clock sample for each date command", async ({
  page,
}) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    await client.send({ type: "Clock", values: [1767225599500, 1767225600000] });
    await client.send({ type: "Init", step_budget: 100000 });
    const sameRun = await client.send({
      type: "Run",
      input: "date '+%Y-%m-%d %H:%M:%S %s'; date '+%Y-%m-%d %H:%M:%S %s'",
    });
    await client.send({ type: "Clock", unix_ms: 1767225601000 });
    const nextRun = await client.send({ type: "Run", input: "date -u '+%F %T %s'" });
    client.close();
    return { sameRun: sameRun.events, nextRun: nextRun.events };
  });

  expect(stdout(result.sameRun)).toBe(
    "2025-12-31 23:59:59 1767225599\n2026-01-01 00:00:00 1767225600\n",
  );
  expect(stdout(result.nextRun)).toBe("2026-01-01 00:00:01 1767225601\n");
  expect(hasExit(result.sameRun, 0)).toBe(true);
  expect(hasExit(result.nextRun, 0)).toBe(true);
});

test("does not fabricate a date when the host callback fails or returns invalid data", async ({
  page,
}) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    await client.send({ type: "Init", step_budget: 100000 });
    const thrown = await client.send({ type: "Clock", mode: "throw" });
    const thrownRun = await client.send({ type: "Run", input: "date '+%s'" });
    const invalid = await client.send({ type: "Clock", mode: "invalid" });
    const invalidRun = await client.send({ type: "Run", input: "date '+%s'" });
    client.close();
    return { thrown, thrownRun: thrownRun.events, invalid, invalidRun: invalidRun.events };
  });

  expect(result.thrown.events).toEqual([]);
  expect(result.invalid.events).toEqual([]);
  expect(hasExit(result.thrownRun, 0)).toBe(false);
  expect(hasExit(result.invalidRun, 0)).toBe(false);
  expect(stdout(result.thrownRun)).toBe("");
  expect(stdout(result.invalidRun)).toBe("");
  expect(result.thrownRun.some((entry: any) => "Stderr" in entry)).toBe(true);
  expect(result.invalidRun.some((entry: any) => "Stderr" in entry)).toBe(true);
});

test("date options are implemented or fail explicitly", async ({ page }) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    await client.send({ type: "Clock", unix_ms: 1767225600000 });
    await client.send({ type: "Init", step_budget: 100000 });
    const supported = await client.send({
      type: "Run",
      input: "date -R; date -Iseconds; date -d '2024-02-29 12:34:56 UTC' '+%F %T'",
    });
    const unsupported = await client.send({ type: "Run", input: "date --not-a-real-option" });
    client.close();
    return { supported: supported.events, unsupported: unsupported.events };
  });

  expect(stdout(result.supported)).toContain("Thu, 01 Jan 2026 00:00:00 +0000");
  expect(stdout(result.supported)).toContain("2026-01-01T00:00:00+00:00");
  expect(stdout(result.supported)).toContain("2024-02-29 12:34:56");
  expect(hasExit(result.supported, 0)).toBe(true);
  expect(hasExit(result.unsupported, 0)).toBe(false);
  expect(result.unsupported.some((entry: any) => "Stderr" in entry)).toBe(true);
});

test("$SECONDS advances from the monotonic clock in the browser worker", async ({
  page,
}) => {
  // Regression: the WASM monotonic path reads performance.now(), which is
  // fractional. An integer-only validator rejected it, so $SECONDS and `time`
  // elapsed were silently stuck at zero.
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    await client.send({ type: "Init", step_budget: 100000 });
    const first = await client.send({ type: "Run", input: "echo $SECONDS" });
    await new Promise((resolve) => setTimeout(resolve, 1100));
    const second = await client.send({ type: "Run", input: "echo $SECONDS" });
    const timed = await client.send({ type: "Run", input: "time true" });
    client.close();
    return {
      first: first.events,
      second: second.events,
      timed: timed.events,
    };
  });

  const firstSeconds = Number(stdout(result.first).trim());
  const secondSeconds = Number(stdout(result.second).trim());
  expect(Number.isNaN(firstSeconds)).toBe(false);
  expect(secondSeconds).toBeGreaterThanOrEqual(firstSeconds + 1);
  // `time` must still emit a real elapsed line, not an empty report.
  expect(stderr(result.timed)).toContain("real");
});

test("AWS SigV4 samples the same host clock used by date", async ({ page }) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    await client.send({ type: "Clock", unix_ms: 1767225600000 });
    await client.send({ type: "Init", step_budget: 100000, allowed_hosts: ["localhost:3100"] });
    const reply = await client.send({
      type: "Run",
      input:
        "curl -sS -v -u AKIAIOSFODNN7EXAMPLE:wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY --aws-sigv4 aws:amz:us-east-1:s3 http://localhost:3100/cors-echo.html",
    });
    client.close();
    return reply.events;
  });

  expect(stderr(result)).toContain("trusted redirect-aware broker");
  expect(result.some((entry: any) => "Exit" in entry && entry.Exit !== 0)).toBe(true);
});
