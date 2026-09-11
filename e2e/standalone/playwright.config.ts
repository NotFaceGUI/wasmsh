import { defineConfig } from "@playwright/test";

const executablePath = process.env.WASMSH_PLAYWRIGHT_EXECUTABLE;

export default defineConfig({
  testDir: "./tests",
  timeout: 60_000,
  retries: 0,
  use: {
    baseURL: "http://localhost:3100",
    ...(executablePath ? { launchOptions: { executablePath } } : {}),
  },
  webServer: {
    command: "npx serve fixture -l 3100 --no-clipboard",
    port: 3100,
    reuseExistingServer: !process.env.CI,
  },
});
