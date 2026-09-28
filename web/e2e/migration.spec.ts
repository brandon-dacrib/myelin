import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

/**
 * The Migration page against the mock (flows.md flow 5): point at Synapse, copy, verify, tick the
 * cutover checklist and cut over, with every state an operator passes through checked by axe.
 * The mock's migration runs on a clock (a six-second copy, a second and a half for each step), so
 * this also shows the page following it without a reload. `e2e-real/migration.spec.ts` does the
 * same against the real binary and a real Synapse database.
 */
test("a migration from Synapse, from the source to the cutover", async ({ page }) => {
  test.setTimeout(90_000);
  const domGuard = installDomNestingGuard(page);
  await signInAsOperator(page);
  await page.goto("/admin/migration");
  await expect(
    page.getByRole("heading", { name: "Migration from Synapse", level: 1 }),
  ).toBeVisible();
  await expect(page.getByRole("button", { name: "Start copying" })).toBeDisabled();
  await expectNoAxeViolations(page, "migration, nothing set");

  const form = page.getByRole("form", { name: "Synapse source" });
  await form.getByLabel(/^Host/).fill("synapse-db.internal");
  await form.getByLabel(/^Password/).fill("s3cret");
  await form.getByLabel(/^Media store path/).fill("/var/lib/synapse/media_store");
  await form.getByRole("button", { name: "Save source" }).click();
  await expect(page.getByText("Set, hidden")).toBeVisible();

  await page.getByRole("button", { name: "Start copying" }).click();
  await expect(page.getByRole("button", { name: "Pause" })).toBeVisible();
  await expectNoAxeViolations(page, "migration, copying");
  await page.getByRole("button", { name: "Pause" }).click();
  await expect(page.getByRole("button", { name: "Resume" })).toBeVisible();
  await page.getByRole("button", { name: "Resume" }).click();

  await expect(page.getByText("Ready for cutover", { exact: true })).toBeVisible({
    timeout: 20_000,
  });
  await page.getByRole("button", { name: "Verify" }).click();
  await expect(page.getByText("Everything matches")).toBeVisible({ timeout: 10_000 });
  await expectNoAxeViolations(page, "migration, verified");

  await page.getByRole("checkbox", { name: "Synapse is stopped" }).check();
  await page.getByRole("checkbox", { name: /will reach this server/ }).check();
  await page.getByRole("button", { name: "Cut over", exact: true }).click();
  const dialog = page.getByRole("dialog");
  await expectNoAxeViolations(page, "migration, cutover confirmation");
  await dialog.getByRole("button", { name: "Cut over now" }).click();
  await expect(page.getByText(/This server is the one in service/)).toBeVisible({
    timeout: 10_000,
  });
  await expectNoAxeViolations(page, "migration, completed");

  domGuard.assertClean();
});
