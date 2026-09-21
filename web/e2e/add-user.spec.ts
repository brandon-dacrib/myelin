import { test, expect } from "@playwright/test";
import {
  signInAsOperator,
  signInReadOnly,
  expectNoAxeViolations,
  installDomNestingGuard,
} from "./utils";

/**
 * Adding an account from the Users page (`src/pages/users/AddUserDialog.tsx`). Registration is
 * closed by default, so this is how everybody after the first administrator gets one.
 */
test.describe("add user", () => {
  test("create an account, hand it over, and find it in the list", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.getByRole("link", { name: "Users" }).first().click();

    await page.getByRole("button", { name: "Add user" }).click();
    const dialog = page.getByRole("dialog", { name: "Add a user" });
    await expect(dialog).toBeVisible();
    await expectNoAxeViolations(page, "add user dialog, empty");

    // Refused first, so the error state is checked too.
    await dialog.getByLabel(/^Username/).fill("alice");
    await dialog.getByRole("button", { name: "Generate" }).click();
    await dialog.getByRole("button", { name: "Create account" }).click();
    await expect(dialog.getByText("@alice:example.org already exists")).toBeVisible();
    await expectNoAxeViolations(page, "add user dialog, username taken");

    await dialog.getByLabel(/^Username/).fill("carol");
    await dialog.getByLabel(/^Display name/).fill("Carol D");
    const password = await dialog.getByLabel(/^Password/).inputValue();
    expect(password).toHaveLength(20);
    await dialog.getByRole("button", { name: "Create account" }).click();

    const done = page.getByRole("dialog", { name: "Account created" });
    await expect(done.getByText("@carol:example.org")).toBeVisible();
    await expect(done.getByText(password)).toBeVisible();
    await expectNoAxeViolations(page, "add user dialog, account created");

    await done.getByRole("button", { name: "Done" }).click();
    await expect(done).toBeHidden();
    await expect(page.getByRole("link", { name: "@carol:example.org" })).toBeVisible();
    domGuard.assertClean();
  });

  test("is not offered to somebody who cannot create accounts", async ({ page }) => {
    await signInReadOnly(page);
    await page.getByRole("link", { name: "Users" }).first().click();
    await expect(page.getByRole("heading", { name: "Users" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Add user" })).toHaveCount(0);
  });
});
