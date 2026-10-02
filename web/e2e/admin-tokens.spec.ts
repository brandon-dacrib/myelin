import { test, expect } from "@playwright/test";
import { expectNoAxeViolations, installDomNestingGuard, signInAsOperator } from "./utils";

/**
 * Admin tokens (Settings, `src/pages/settings/AdminTokensPage.tsx`): mint a token with chosen
 * scopes, see it once, find it in the list with its scopes, revoke it.
 */
test.describe("admin tokens", () => {
  test("mint a bridges:read token, see it listed with its scope, revoke it", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/settings/admin-tokens");
    await expect(page.getByRole("heading", { name: "Admin tokens" })).toBeVisible();
    const seeded = page.getByRole("row", { name: /Bridge team dashboard/ });
    await expect(seeded.getByText("bridges:read")).toBeVisible();
    await expect(
      page.getByRole("row", { name: /Deploy pipeline/ }).getByText("admin:write"),
    ).toBeVisible();
    await expectNoAxeViolations(page, "admin tokens");

    await page.getByRole("button", { name: "Mint token" }).click();
    const dialog = page.getByRole("dialog", { name: "Mint an admin token" });
    await expect(dialog).toBeVisible();
    await expect(dialog.getByText(/See the bridges, their registrations/)).toBeVisible();
    await expectNoAxeViolations(page, "mint admin token dialog");

    await dialog.getByLabel(/^Name/).fill("e2e bridge watcher");
    await dialog.getByRole("checkbox", { name: /^admin:write/ }).uncheck();
    await dialog.getByRole("checkbox", { name: /^admin:read/ }).uncheck();
    await dialog.getByRole("checkbox", { name: /^bridges:read/ }).check();
    await expect(dialog.getByText("The token will hold bridges:read.")).toBeVisible();
    await dialog.getByRole("radio", { name: "7 days" }).check();
    await dialog.getByRole("button", { name: "Mint token" }).click();

    const done = page.getByRole("dialog", { name: "Admin token ready" });
    await expect(done.getByTestId("token")).toHaveText(/^hsa_/);
    await expect(done.getByText("bridges:read")).toBeVisible();
    await expectNoAxeViolations(page, "admin token ready");
    await done.getByRole("button", { name: "Done" }).click();

    const row = page.getByRole("row", { name: /e2e bridge watcher/ });
    await expect(row.getByText("bridges:read")).toBeVisible();
    await expect(row.getByText("in 7 days")).toBeVisible();

    await row.getByRole("button", { name: "Revoke e2e bridge watcher" }).click();
    await page.getByRole("button", { name: "Revoke token" }).click();
    await expect(page.getByRole("row", { name: /e2e bridge watcher/ })).toBeHidden();
    domGuard.assertClean();
  });
});
