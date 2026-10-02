import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

/**
 * Editing a bridge's registration and testing its connection from its page
 * (`src/pages/bridges/EditBridgeDialog.tsx`, "Test connection" in `BridgeDetailPage.tsx`).
 */
test.describe("edit bridge", () => {
  test("test the connection, then change rate limiting and add a namespace rule", async ({
    page,
  }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/bridges/whatsapp");
    await expect(page.getByRole("heading", { name: "WhatsApp" })).toBeVisible();
    await expect(page.getByText("None: the bridge speaks only as its own bot user.")).toBeVisible();

    await page.getByRole("button", { name: "Test connection" }).click();
    await expect(page.getByText("WhatsApp answered").first()).toBeVisible();

    await page.getByRole("button", { name: "Edit", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "Edit WhatsApp" });
    await expect(dialog.getByText(/tokens and bot name are not changed here/)).toBeVisible();
    await expectNoAxeViolations(page, "edit bridge dialog");
    await dialog.getByRole("switch", { name: "Rate limited" }).click();
    await dialog.getByRole("button", { name: "Add rule" }).nth(1).click();
    await dialog.getByLabel("Alias namespaces pattern").fill("#wa_.*:example\\.org");
    await dialog.getByRole("button", { name: "Save" }).click();
    await expect(dialog).toBeHidden();

    await expect(page.getByText("Yes", { exact: true })).toBeVisible();
    await expect(page.getByText("#wa_.*:example\\.org")).toBeVisible();
    await expect(page.getByText("exclusive", { exact: true })).toBeVisible();
    await expectNoAxeViolations(page, "bridge page with namespaces");
    domGuard.assertClean();
  });

  test("a bridge that is down does not answer, and the page says why", async ({ page }) => {
    await signInAsOperator(page);
    await page.goto("/admin/bridges/signal");
    await page.getByRole("button", { name: "Test connection" }).click();
    await expect(page.getByText("Signal did not answer").first()).toBeVisible();
    await expect(page.getByText(/Connection refused/)).toBeVisible();
  });
});
