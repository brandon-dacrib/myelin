import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

// Erasing a user on the mock: the deactivate dialog's "Also erase their data" box, the
// explanation that appears with it, the Erased badge on the page and in the list, a page with
// nothing left to reactivate, and the erase box on an account that was only deactivated.

const BOT = "@bot:example.org";
const SPAMMER = "@spammer42:example.org";

test.describe("User erasure", () => {
  test("deactivate and erase in one step, then nothing is left to reactivate", async ({ page }) => {
    const guard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page
      .getByRole("navigation", { name: "Primary" })
      .getByRole("link", { name: "Users" })
      .click();
    await page.getByRole("link", { name: BOT }).click();
    await expect(page.getByRole("heading", { name: "Notice bot", level: 1 })).toBeVisible();

    await page.getByRole("button", { name: "Deactivate" }).click();
    const dialog = page.getByRole("dialog", { name: `Deactivate ${BOT}?` });
    const erase = dialog.getByRole("checkbox", { name: /Also erase their data/ });
    await expect(erase).not.toBeChecked();
    await expect(dialog.getByRole("button", { name: "Deactivate", exact: true })).toBeVisible();
    await erase.check();
    await expect(dialog.getByText(/every device, with its encryption keys/)).toBeVisible();
    await expect(dialog.getByText(/messages they sent/)).toBeVisible();
    await expectNoAxeViolations(page, "deactivate dialog with erase");
    await dialog.getByRole("button", { name: "Deactivate and erase" }).click();
    await expect(dialog).toBeHidden();

    // The page: both badges, the id as the heading (the name is gone), no devices, no way back.
    await expect(page.getByText("Erased", { exact: true })).toBeVisible();
    await expect(page.getByText("Deactivated", { exact: true })).toBeVisible();
    await expect(page.getByRole("heading", { name: BOT, level: 1 })).toBeVisible();
    await expect(page.getByText("No devices.")).toBeVisible();
    await expect(page.getByText("This account was erased")).toBeVisible();
    await expect(page.getByRole("button", { name: "Reactivate" })).toBeHidden();
    await expect(page.getByRole("button", { name: "Deactivate" })).toBeHidden();
    await expect(page.getByRole("button", { name: "Reset password" })).toBeDisabled();
    await expectNoAxeViolations(page, "erased user page");

    // The list: the badge beside Deactivated.
    await page.getByRole("link", { name: "Users" }).first().click();
    const row = page.getByRole("row").filter({ hasText: BOT });
    await expect(row.getByText("Deactivated", { exact: true })).toBeVisible();
    await expect(row.getByText("Erased", { exact: true })).toBeVisible();
    guard.assertClean();
  });

  test("an account that was only deactivated can be erased afterwards", async ({ page }) => {
    await signInAsOperator(page);
    await page.goto(`/admin/users/${encodeURIComponent(SPAMMER)}`);
    await page.getByRole("button", { name: "Deactivate" }).click();
    await page
      .getByRole("dialog", { name: `Deactivate ${SPAMMER}?` })
      .getByRole("button", { name: "Deactivate", exact: true })
      .click();
    await expect(page.getByRole("button", { name: "Reactivate" })).toBeVisible();
    await expect(page.getByText("Erased", { exact: true })).toBeHidden();

    await page.getByRole("button", { name: "Erase data" }).click();
    const dialog = page.getByRole("dialog", { name: `Erase ${SPAMMER}'s data?` });
    await expect(dialog.getByText(/single-sign-on links/)).toBeVisible();
    await expectNoAxeViolations(page, "erase dialog");
    await dialog.getByRole("button", { name: "Erase data" }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByText("Erased", { exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Reactivate" })).toBeHidden();
    await expect(page.getByRole("button", { name: "Erase data" })).toBeHidden();
  });
});
