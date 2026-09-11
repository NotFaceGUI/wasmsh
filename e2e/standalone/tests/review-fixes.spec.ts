import { test, expect } from "@playwright/test";

/**
 * Browser coverage for the final-review fixes:
 *  - deterministic default environment (HOME/PWD/PATH) and `cd`/`~`
 *  - `timeout` refuses instead of silently succeeding
 *  - bundled utilities are discoverable via `command -v`/`type`
 *  - an invalid network policy fails closed at the WASM boundary
 */

function decode(events: any[], key: "Stdout" | "Stderr"): string {
  const parts: number[] = [];
  for (const event of events) {
    if (event && typeof event === "object" && key in event) {
      parts.push(...event[key]);
    }
  }
  return new TextDecoder().decode(new Uint8Array(parts));
}

const stdout = (events: any[]) => decode(events, "Stdout");
const stderr = (events: any[]) => decode(events, "Stderr");
const exit = (events: any[]) =>
  events.find((event) => "Exit" in event)?.Exit ?? null;

test("init seeds a deterministic HOME/PWD/PATH so cd and ~ work", async ({
  page,
}) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    await client.send({ type: "Init", step_budget: 100000 });
    const reply = await client.send({
      type: "Run",
      input: "echo $HOME; echo $PWD; cd; echo $PWD; echo ~",
    });
    client.close();
    return reply.events;
  });

  expect(stdout(result)).toBe("/home/user\n/\n/home/user\n/home/user\n");
  expect(exit(result)).toBe(0);
});

test("timeout refuses rather than printing a fake success", async ({ page }) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    await client.send({ type: "Init", step_budget: 100000 });
    const reply = await client.send({ type: "Run", input: "timeout 5 echo hi" });
    client.close();
    return reply.events;
  });

  expect(exit(result)).toBe(125);
  expect(stdout(result)).toBe("");
  expect(stderr(result)).toContain("not supported");
});

test("bundled utilities are discoverable via command -v and type", async ({
  page,
}) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    await client.send({ type: "Init", step_budget: 100000 });
    const reply = await client.send({
      type: "Run",
      input: "command -v curl; command -v jq; type -t curl; type curl",
    });
    client.close();
    return reply.events;
  });

  expect(stdout(result)).toBe("curl\njq\nutility\ncurl is a shell utility\n");
  expect(exit(result)).toBe(0);
});

test("an invalid network rule fails closed at init", async ({ page }) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    const init = await client.send({
      type: "Init",
      step_budget: 100000,
      network_policy: {
        enabled: true,
        default_action: "deny",
        allow: ["api.*.example.com"],
        deny: [],
      },
    });
    const run = await client.send({
      type: "Run",
      input: "curl -sS http://localhost:3100/cors-echo.html",
    });
    client.close();
    return { init: init.events, run: run.events };
  });

  const error = result.init.find(
    (event: any) =>
      Array.isArray(event.Diagnostic) &&
      event.Diagnostic[0] === "Error" &&
      /invalid network policy/.test(event.Diagnostic[1]),
  );
  expect(error).toBeTruthy();
  expect(exit(result.run)).not.toBe(0);
});

test("legacy and structured network config cannot both be supplied", async ({
  page,
}) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    const init = await client.send({
      type: "Init",
      step_budget: 100000,
      allowed_hosts: ["example.com"],
      network_policy: {
        enabled: true,
        default_action: "deny",
        allow: ["example.com"],
        deny: [],
      },
    });
    client.close();
    return init.events;
  });

  expect(
    result.some(
      (event: any) =>
        Array.isArray(event.Diagnostic) &&
        event.Diagnostic[0] === "Error" &&
        /cannot both be configured/.test(event.Diagnostic[1]),
    ),
  ).toBe(true);
});
