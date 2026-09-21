import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

/**
 * The Configuration pages are where an operator changes how the server behaves, and until this
 * spec they were the only flow in the interface that had never been through axe
 * (docs/next-steps.md's known gaps). Each state an operator passes through on the way to a saved
 * change is checked: the index, a search, a section's generated form, an edited field, the
 * review dialog, a change the server would reject, and the saved result -- at desktop width, and
 * the two densest states again at phone width.
 */
test.describe("configuration", () => {
  test("find a setting, change it, review it, save it", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);

    await page.getByRole("link", { name: "Configuration" }).first().click();
    await expect(page.getByRole("heading", { name: "Configuration", level: 1 })).toBeVisible();
    await expect(page.getByRole("link", { name: "Rate limits" })).toBeVisible();
    await expectNoAxeViolations(page, "configuration index");

    await page.getByLabel("Search settings").fill("registration");
    await expect(page.getByText("auth.enable_registration")).toBeVisible();
    await expectNoAxeViolations(page, "configuration index, search results");

    await page.getByLabel("Search settings").fill("");
    // The sidebar has a "Federation" of its own; the section is the one in the page.
    await page.getByRole("main").getByRole("link", { name: "Federation" }).click();
    const timeout = page.getByRole("textbox", { name: "Client timeout" });
    await expect(timeout).toBeVisible();
    await expectNoAxeViolations(page, "configuration section, untouched form");

    // A change the server would refuse, checked without saving.
    await timeout.fill("forever");
    await timeout.blur();
    await page.getByRole("button", { name: "Review and save" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toBeVisible();
    await expectNoAxeViolations(page, "configuration review dialog");
    await dialog.getByRole("button", { name: "Check without saving" }).click();
    await expect(dialog.getByText("The server would reject this:")).toBeVisible();
    await expectNoAxeViolations(page, "configuration review dialog, rejected check");
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden();

    // And one it accepts.
    await timeout.fill("90s");
    await timeout.blur();
    await expectNoAxeViolations(page, "configuration section, edited field");
    await page.getByRole("button", { name: "Review and save" }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Save changes" }).click();
    await expect(page.getByText("Federation saved")).toBeVisible();
    await expectNoAxeViolations(page, "configuration section, saved");

    domGuard.assertClean();
  });

  test("the index and a section's form hold up at phone width", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.setViewportSize({ width: 390, height: 844 });

    await page.goto("/admin/configuration");
    await expect(page.getByRole("link", { name: "Rate limits" })).toBeVisible();
    await expectNoAxeViolations(page, "configuration index, phone width");

    await page.getByRole("link", { name: "Rate limits" }).click();
    await expect(page.getByText("rate_limits.login.burst_count")).toBeVisible();
    await expectNoAxeViolations(page, "configuration section, phone width");

    domGuard.assertClean();
  });
});
