import { defineConfig, devices } from "@playwright/test";

/**
 * Browser end-to-end suite against a running deployment: the local stack
 * (`task dev:*`) by default, or any release via the WNK_E2E_* variables.
 */
const chromium = process.env.WNK_E2E_CHROMIUM;

export default defineConfig({
  testDir: "e2e",
  fullyParallel: false,
  workers: 1,
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [["list"], ["html", { open: "never" }]] : "list",
  use: {
    baseURL: process.env.WNK_E2E_WEB_URL ?? "http://localhost:5173",
    trace: "retain-on-failure",
    launchOptions: chromium ? { executablePath: chromium } : {},
  },
  projects: [
    {
      name: "desktop",
      use: { ...devices["Desktop Chrome"] },
      grepInvert: /@mobile/,
    },
    { name: "phone", use: { ...devices["Pixel 7"] }, grep: /@mobile/ },
  ],
});
