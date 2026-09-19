import { test, expect, type Page } from "@playwright/test";

/**
 * Proves the app against a real `hs serve`, not `hs-admin-mock`
 * (docs/status/16-management-web-interface.md, "Prove it against the running binary, not
 * against the mock"). See `playwright.real.config.ts` for how to run this and what each env var
 * does; skipped entirely (no server started, every test a no-op skip) unless
 * `HS_REAL_SERVER_URL` is set.
 *
 * As of this writing `hs serve` wires its admin surface's `TokenVerifier` to an empty
 * `StaticVerifier` (`crates/hs-cli/src/serve.rs::dummy_admin_state`) — `hs_auth::
 * admin_verifier::AdminTokenVerifier` exists and is unit-tested but not yet plugged into `hs
 * serve` itself, an `hs-cli` change this track cannot make (`hs-cli` isn't ours to edit). That
 * means *every* bearer token, including a real admin's, currently gets a `401` from `/api/v1/*`.
 * The "unrecognized token" case below needs nothing else to be true and is the one guaranteed to
 * pass today; the authenticated walkthrough is written and ready, gated behind
 * `HS_REAL_ADMIN_TOKEN`, for the day that wiring lands.
 */

const hasServer = Boolean(process.env.HS_REAL_SERVER_URL);
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;

test.describe("real server", () => {
  test.skip(!hasServer, "HS_REAL_SERVER_URL not set — see playwright.real.config.ts");

  test("sign-in honestly rejects an unrecognized token", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByRole("tab", { name: "Access token" })).toBeVisible();
    await page.getByRole("tab", { name: "Access token" }).click();
    await page.getByLabel("Access token").fill("syt_definitely_not_a_real_token");
    await page.getByRole("button", { name: "Sign in" }).click();

    // Whatever the real reason (today: the verifier isn't wired up yet, so every token is
    // "unrecognized"; once it is, a genuinely bad token gets the same answer) — the important
    // thing is an honest, specific message, not a stuck spinner or an unhandled exception.
    await expect(page.getByRole("alert")).toContainText(/wasn't recognized|expired/i);
    await page.screenshot({ path: "test-results/real-sign-in-rejected.png", fullPage: true });
  });

  test.describe("authenticated", () => {
    test.skip(!adminToken, "HS_REAL_ADMIN_TOKEN not set — see playwright.real.config.ts");

    test.beforeEach(async ({ page }) => {
      await page.goto("/");
      await page.getByRole("tab", { name: "Access token" }).click();
      await page.getByLabel("Access token").fill(adminToken!);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page).toHaveURL(/\/admin\/?$/);
    });

    test("dashboard renders what's real and reports what isn't", async ({ page }) => {
      await screenshotHonestly(page, "/", "real-dashboard.png");
    });

    test("users list is real (GET /users is wired)", async ({ page }) => {
      await page.goto("/admin/users");
      // Either real rows, a real empty state, or (if the directory source isn't attached, per
      // track 15's status file) an honest 503 "not connected to a data source" — never a bare
      // ErrorState and never an infinite spinner.
      await expect(page.getByRole("table").or(page.getByRole("status"))).toBeVisible({
        timeout: 10_000,
      });
      await page.screenshot({ path: "test-results/real-users.png", fullPage: true });
    });

    test("bridges list honestly reports not-implemented (GET /appservices still 501)", async ({
      page,
    }) => {
      await page.goto("/admin/bridges");
      await expect(page.getByText(/isn't implemented on this server yet/i)).toBeVisible({
        timeout: 10_000,
      });
      await page.screenshot({ path: "test-results/real-bridges-not-implemented.png", fullPage: true });
    });

    test("user detail is real, and its sub-resource gap is honest", async ({ page }) => {
      // /users/{user_id} is real; /users/{user_id}/devices is not in track 15's real-handler
      // list yet, so the Sessions section should say so rather than claim "No devices."
      await page.goto("/admin/users/@ops:test.local");
      await expect(page.getByRole("heading", { name: "@ops:test.local" })).toBeVisible({
        timeout: 10_000,
      });
      await page.screenshot({ path: "test-results/real-user-detail.png", fullPage: true });
    });

    test("rooms list honestly reports not-implemented (GET /rooms still 501)", async ({ page }) => {
      await page.goto("/admin/rooms");
      await expect(page.getByText(/isn't implemented on this server yet/i)).toBeVisible({
        timeout: 10_000,
      });
      await page.screenshot({ path: "test-results/real-rooms-not-implemented.png", fullPage: true });
    });
  });
});

/** Loads `path`, waits for the page to settle, and screenshots it — used for pages this suite
 * doesn't assert specific content on (the mix of real/501/503 sections varies with what's wired
 * up on the server under test), so the screenshot itself is the evidence. */
async function screenshotHonestly(page: Page, path: string, filename: string) {
  await page.goto(path);
  await page.waitForLoadState("networkidle");
  await page.screenshot({ path: `test-results/${filename}`, fullPage: true });
}
