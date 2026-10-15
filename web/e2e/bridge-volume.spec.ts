import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

/**
 * A bridge this server deploys whose volume filled (the demo's Slack bridge, 2026-10-15: its
 * init container failed with `No space left on device` and the Registrations page only said
 * Degraded). The list says the volume is full at a glance, the bridge's page says how full and
 * what to do, and a larger size is set from there (`src/pages/bridges/BridgeVolume.tsx`).
 */
test.describe("bridge volume", () => {
  test("a full volume is seen on the list and grown from the bridge's page", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/bridges/registrations");
    const row = page
      .getByRole("row")
      .filter({ has: page.getByRole("link", { name: "Slack Brandon", exact: true }) });
    await expect(row.getByText("Degraded")).toBeVisible();
    await expect(row.getByText("Volume full")).toBeVisible();
    await expectNoAxeViolations(page, "bridges list with a full volume");

    await row.getByRole("link", { name: "Slack Brandon" }).click();
    await expect(page.getByRole("heading", { name: "Slack Brandon" })).toBeVisible();
    const alert = page.getByTestId("volume-alert");
    await expect(alert).toContainText("No space left on device");
    const volume = page.getByRole("region", { name: "Volume" });
    await expect(volume.getByText("1003.5 MiB of 1.0 GiB used (98%)")).toBeVisible();
    await expect(volume.getByText(/holds the bridge's database and configuration/)).toBeVisible();
    await expectNoAxeViolations(page, "bridge page with a full volume");

    // A smaller size is refused by the server, beside the field.
    const field = volume.getByRole("textbox", { name: "New size" });
    await field.fill("512Mi");
    await volume.getByRole("button", { name: "Set size" }).click();
    await expect(volume.getByRole("alert")).toContainText("less than 1Gi");

    await field.fill("4Gi");
    await volume.getByRole("button", { name: "Set size" }).click();
    await expect(alert).toBeHidden();
    await expect(volume.getByText(/of 4\.0 GiB used \(25%\)/)).toBeVisible();
    domGuard.assertClean();
  });

  test("the offering page shows each bridge's volume and sizes it from a dialog", async ({
    page,
  }) => {
    await signInAsOperator(page);
    await page.goto("/admin/bridges/offerings/mautrix-slack");
    const table = page.getByRole("table");
    await expect(table.getByText("Volume full")).toBeVisible();
    await table.getByRole("button", { name: "Volume for @brandon:example.org" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.getByText("1003.5 MiB of 1.0 GiB used (98%)")).toBeVisible();
    await expectNoAxeViolations(page, "bridge volume dialog");
    await dialog.getByRole("textbox", { name: "New size" }).fill("10Gi");
    await dialog.getByRole("button", { name: "Set size" }).click();
    await expect(dialog).toBeHidden();
    await expect(table.getByText("Volume 1003.5 MiB of 10.0 GiB used (10%)")).toBeVisible();
  });
});
