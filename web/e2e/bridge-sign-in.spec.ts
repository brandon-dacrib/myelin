import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

/**
 * Who has signed in to a bridge, where an operator looks for it: each person's own bridge on the
 * offering page, and the Sign in tab of a bridge registered before the server kept provisioning
 * secrets, which offers to add the secret instead of leaving the operator a merge patch to write.
 */
test.describe("bridge sign-ins", () => {
  test("the offering page says who has signed in to their bridge", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/bridges/offerings/mautrix-whatsapp");
    const table = page.getByRole("table", { name: /People's .* bridges/ });
    await expect(table.getByRole("columnheader", { name: /^Signed in to/ })).toBeVisible();
    const alice = table.getByRole("row").filter({ hasText: "@alice:example.org" });
    await expect(alice.getByText("+1 555-123-4567")).toBeVisible();
    const ops = table.getByRole("row").filter({ hasText: "@ops:example.org" });
    await expect(ops.getByText("Not signed in", { exact: true })).toBeVisible();
    await expectNoAxeViolations(page, "bridge offering, sign-ins");
    domGuard.assertClean();
  });

  test("an older registration gets its provisioning secret from the Sign in tab", async ({
    page,
  }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/bridges/telegram#sign-in");
    await expect(page.getByText(/it needs the bridge's provisioning secret/)).toBeVisible();
    await expectNoAxeViolations(page, "bridge sign-in tab, no provisioning secret");

    await page.getByLabel("Provisioning secret").fill("0123abcd");
    await page.getByRole("button", { name: "Save secret" }).click();
    await expect(page.getByText("Provisioning secret saved", { exact: true })).toBeVisible();
    // The server can ask now: a shared bridge asks whom to ask about.
    await expect(page.getByText(/Many people can use this bridge/)).toBeVisible();
    domGuard.assertClean();
  });
});
