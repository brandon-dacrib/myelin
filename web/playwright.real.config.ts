import { defineConfig, devices } from "@playwright/test";

/**
 * Playwright configuration for `e2e-real/`: the suite that proves the app against a *real*
 * running `hs serve`, not `hs-admin-mock` (docs/status/16-management-web-interface.md, "Prove it
 * against the running binary, not against the mock"). Separate from `playwright.config.ts`
 * (testDir `./e2e`, which `npm run test:e2e` runs and which must stay green with no server
 * running at all) so this suite never runs by accident.
 *
 * Skipped by default: with `HS_REAL_SERVER_URL` unset, no `webServer` is started and every spec
 * in `e2e-real/` calls `test.skip()` immediately (see `e2e-real/real-server.spec.ts`). To run it:
 *
 *   HS_REAL_SERVER_URL=http://127.0.0.1:8095 \
 *   HS_REAL_ADMIN_TOKEN=syt_... \
 *     npm run test:e2e:real
 *
 * `HS_REAL_ADMIN_TOKEN` is optional; without it, only the always-available "sign-in rejects an
 * unrecognized token" case runs (which needs no admin account — see the spec for why that's the
 * one case guaranteed to be exercisable against the server as of this writing).
 */
const target = process.env.HS_REAL_SERVER_URL;
const port = 4180;

export default defineConfig({
  testDir: "./e2e-real",
  fullyParallel: false,
  workers: 1,
  reporter: [["list"], ["html", { open: "never", outputFolder: "playwright-report-real" }]],
  use: {
    baseURL: `http://localhost:${port}/admin/`,
    trace: "on",
    screenshot: "on",
  },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  webServer: target
    ? {
        command: `VITE_HS_API_PROXY_TARGET=${target} npm run dev:real`,
        url: `http://localhost:${port}/admin/`,
        reuseExistingServer: true,
        timeout: 60_000,
      }
    : undefined,
});
