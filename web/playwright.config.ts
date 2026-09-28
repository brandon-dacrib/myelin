import { defineConfig, devices } from "@playwright/test";

/**
 * Playwright configuration for the management web interface end-to-end suite.
 * Runs against the Vite dev server with MSW mocking the admin API (`npm run dev:mock`),
 * so the suite needs neither a homeserver nor Docker.
 *
 * `HS_E2E_PORT` moves the preview server off 4173. Two checkouts running the suite at once
 * (parallel worktrees) would otherwise share one server -- `reuseExistingServer` would quietly
 * test the other checkout's build -- so give each its own port.
 */
const port = Number(process.env.HS_E2E_PORT ?? 4173);

export default defineConfig({
  testDir: "./e2e",
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  workers: process.env.CI ? 1 : undefined,
  reporter: [["html", { open: "never" }]],
  use: {
    baseURL: `http://localhost:${port}/admin/`,
    trace: "on-first-retry",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: {
    command: `npm run preview:mock -- --port ${port} --strictPort`,
    url: `http://localhost:${port}/admin/`,
    reuseExistingServer: !process.env.CI,
    timeout: 60_000,
  },
});
