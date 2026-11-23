import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  testMatch: "workbench.spec.ts",
  timeout: 30_000,
  retries: 0,
  use: {
    baseURL: "http://127.0.0.1:4173",
    channel: "chrome",
    headless: true,
    viewport: { width: 1440, height: 900 },
  },
  webServer: {
    command: "pnpm --filter @openmirasim/web preview --host 127.0.0.1 --port 4173",
    url: "http://127.0.0.1:4173",
    reuseExistingServer: !process.env.CI,
    timeout: 20_000,
  },
});
