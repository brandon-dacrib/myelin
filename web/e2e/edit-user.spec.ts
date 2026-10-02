import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

/**
 * Editing an account from its page (`src/pages/users/EditUserDialog.tsx`): server administrator
 * can now be granted or revoked after the account was made, and the display name, avatar and
 * kind of account changed.
 */
test.describe("edit user", () => {
  test("grant administrator and rename, and the page shows both", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/users/%40alice%3Aexample.org");
    await expect(page.getByRole("heading", { name: "Alice" })).toBeVisible();
    await expect(page.getByText("Admin", { exact: true })).toHaveCount(0);

    await page.getByRole("button", { name: "Edit" }).click();
    const dialog = page.getByRole("dialog", { name: "Edit account" });
    await expect(dialog.getByText("Only what you change is sent.")).toBeVisible();
    await expectNoAxeViolations(page, "edit account dialog");

    await dialog.getByLabel(/^Display name/).fill("Alice Liddell");
    await dialog.getByRole("switch", { name: /Server administrator/ }).click();
    await dialog.getByRole("button", { name: "Save" }).click();
    await expect(dialog).toBeHidden();

    await expect(page.getByRole("heading", { name: "Alice Liddell" })).toBeVisible();
    await expect(page.getByText("Admin", { exact: true })).toBeVisible();
    domGuard.assertClean();
  });
});
