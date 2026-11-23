import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: ".",
  testMatch: "fullstack.spec.ts",
  timeout: 45_000,
  retries: 0,
  workers: 1,
  use: {
    channel: "chrome",
    headless: true,
    viewport: { width: 1440, height: 900 },
  },
});
