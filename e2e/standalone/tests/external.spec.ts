/**
 * Standalone browser external registration contract.
 *
 * A browser worker can retain registrations and answer capability queries,
 * but this fixture deliberately has no native-process executor. Running a
 * registered native executable must therefore fail with 126.
 */
import { test, expect } from "@playwright/test";

function events(reply: any) {
  return reply.events ?? [];
}

function exitCode(reply: any) {
  return events(reply).find((event: any) => Object.hasOwn(event, "Exit"))?.Exit;
}

test("register, query, and unregister external commands", async ({ page }) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    await client.send({ type: "Init", step_budget: 0 });
    const registered = await client.send({
      type: "RegisterExternal",
      name: "hostcat",
      executable: "/trusted/hostcat",
      options: {
        cwd: "/trusted/work",
        env: { TEST_ONLY: "yes" },
        max_input_bytes: 1024,
        max_output_bytes: 1024,
        timeout_ms: 1000,
      },
    });
    const query = await client.send({ type: "ExternalCommands" });
    const run = await client.send({ type: "Run", input: "hostcat" });
    const unregistered = await client.send({
      type: "UnregisterExternal",
      name: "hostcat",
    });
    client.close();
    return { registered, query, run, unregistered };
  });

  expect(events(result.registered)).toEqual([]);
  expect(result.query.events).toEqual([{ ExternalCommands: ["hostcat"] }]);
  expect(exitCode(result.run)).toBe(126);
  expect(result.unregistered.events).toEqual([{ ExternalUnregistered: true }]);
});

test("unregistered external remains 127", async ({ page }) => {
  await page.goto("/");
  const result = await page.evaluate(async () => {
    const client = (window as any).createShellWorkerClient();
    await client.send({ type: "Init", step_budget: 0 });
    const run = await client.send({ type: "Run", input: "hostcat" });
    client.close();
    return run;
  });
  expect(exitCode(result)).toBe(127);
});

