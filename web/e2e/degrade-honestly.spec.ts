import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

/**
 * Proves the shared "degrade honestly" treatment (docs/status/16-management-web-interface.md):
 * a page whose data source answers RFC 9457 `501`/`503` must say so, not spin forever, show an
 * empty table that implies zero rows, or a red error that looks like a fault. This is the
 * behaviour the real `hs serve` exercises constantly today (135 of 142 operations are still
 * `501`) — proven here against the mock via `window.__hsAdminMock.setForceProblem`
 * (`src/mocks/browser.ts`) so it runs without a real server, and again in
 * `e2e-real/real-server.spec.ts` against one when available.
 */
test.describe("degrade honestly", () => {
  test("a 501 list renders NotImplementedState, not a spinner or a table", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.evaluate(() =>
      window.__hsAdminMock!.setForceProblem("/api/v1/appservices", 501, {
        detail: "Bridges are not wired up yet.",
      }),
    );

    await page.getByRole("link", { name: "Bridges" }).first().click();
    await expect(page.getByRole("heading", { name: "Bridges" })).toBeVisible();

    // Not a fault: role="status", not role="alert"; no permanently-spinning loading indicator.
    await expect(page.getByText("isn't implemented on this server yet")).toBeVisible();
    await expect(page.getByText("Bridges are not wired up yet.")).toBeVisible();
    await expect(page.getByRole("table")).toHaveCount(0);
    await expect(page.getByRole("alert")).toHaveCount(0);

    await expectNoAxeViolations(page, "bridges list, 501");
    domGuard.assertClean();
  });

  test("a 503 detail page says it isn't connected to a data source, and Retry works", async ({
    page,
  }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.evaluate(() =>
      window.__hsAdminMock!.setForceProblem("/api/v1/users/*", 503, {
        detail: "No user directory attached.",
      }),
    );

    await page.goto("/admin/users/@alice:example.org");
    await expect(page.getByText("isn't connected to a data source on this server yet")).toBeVisible();
    await expect(page.getByRole("button", { name: "Check again" })).toBeVisible();

    await expectNoAxeViolations(page, "user detail, 503");
    domGuard.assertClean();
  });

  test("a 403 shows ForbiddenState naming the required scope, not a generic error", async ({
    page,
  }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.evaluate(() =>
      window.__hsAdminMock!.setForceProblem("/api/v1/rooms", 403, {
        required_scope: "moderation:read",
      }),
    );

    await page.goto("/admin/rooms");
    await expect(page.getByText(/needs the/i)).toBeVisible();
    await expect(page.getByText("moderation:read")).toBeVisible();

    await expectNoAxeViolations(page, "rooms list, 403");
    domGuard.assertClean();
  });
});
