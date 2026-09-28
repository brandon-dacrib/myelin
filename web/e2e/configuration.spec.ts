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
    await expect(page.getByText("Federation saved", { exact: true })).toBeVisible();
    await expectNoAxeViolations(page, "configuration section, saved");

    domGuard.assertClean();
  });

  test("a list of objects is edited as a form per entry, never as JSON", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);

    await page.goto("/admin/configuration/media");
    const sizes = page.getByRole("group", { name: "Thumbnail sizes", exact: true });
    await expect(sizes).toBeVisible();
    // Decision 0010: nothing on the page is a text box for a file format.
    await expect(page.locator("textarea")).toHaveCount(0);
    await expectNoAxeViolations(page, "configuration section, structured editors");

    // Add an entry from the keyboard, and fill it in.
    await sizes.getByRole("button", { name: "Add thumbnail size" }).click();
    const added = page.getByRole("group", { name: /^Thumbnail size 6/ });
    await expect(added.getByLabel(/^Width/)).toBeFocused();
    await page.keyboard.type("1024");
    await added.getByLabel(/^Height/).fill("768");
    await added.getByLabel(/^Method/).click();
    await page.getByRole("option", { name: "Scale" }).click();
    await expectNoAxeViolations(page, "configuration section, a new list entry");

    // Move it to the top, from the keyboard.
    await added.getByRole("button", { name: "Move Thumbnail size 6 up" }).focus();
    for (let i = 0; i < 5; i += 1) await page.keyboard.press("Enter");
    await expect(
      page.getByRole("group", { name: /^Thumbnail size 1 · 1024 · 768 · scale/ }),
    ).toBeVisible();

    // The storage backend is a variant: choosing S3 shows S3's own settings.
    await page.getByLabel("Backend").click();
    await page.getByRole("option", { name: "S3" }).click();
    await page.getByLabel(/^Bucket/).fill("media");
    await expectNoAxeViolations(page, "configuration section, a variant switched");

    await page.getByRole("button", { name: "Review and save" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.getByText("6 entries")).toBeVisible();
    await dialog.getByRole("button", { name: "Save changes" }).click();
    await expect(page.getByText("Media saved", { exact: true })).toBeVisible();
    await expect(
      page.getByRole("group", { name: /^Thumbnail size 1 · 1024 · 768 · scale/ }),
    ).toBeVisible();

    domGuard.assertClean();
  });

  test("listeners are set at install: shown as the replica runs them, never offered for edit", async ({
    page,
  }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);

    await page.goto("/admin/configuration/listeners");
    await expect(page.getByRole("heading", { name: "Listeners", level: 1 })).toBeVisible();
    await expect(page.getByText("Bootstrap only")).toBeVisible();
    await expect(page.getByText("Set at install", { exact: true })).toBeVisible();
    await expect(page.getByText("client, federation, media, health, admin")).toBeVisible();
    // Decision 0010: nothing here can be stored, so nothing is offered for edit.
    await expect(page.getByRole("checkbox")).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Review and save" })).toHaveCount(0);
    await expectNoAxeViolations(page, "configuration section, listeners");

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
