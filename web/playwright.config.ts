import { defineConfig, devices } from "@playwright/test";

/**
 * Playwright configuration for the management web interface end-to-end suite.
 * Runs against the Vite dev server with MSW mocking the admin API (`npm run dev:mock`),
 * so the suite needs neither a homeserver nor Docker.
 */
export default defineConfig({
  testDir: "./e2e",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: process.env.CI ? 1 : undefined,
  reporter: [["html", { open: "never" }]],
  use: {
    baseURL: "http://localhost:4173/admin/",
    trace: "on-first-retry",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    command: "npm run preview:mock -- --port 4173",
    url: "http://localhost:4173/admin/",
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
  },
});
