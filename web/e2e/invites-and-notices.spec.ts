import { test, expect } from "@playwright/test";
import { expectNoAxeViolations, installDomNestingGuard, signInAsOperator } from "./utils";

/**
 * Invite links and server notices (Settings, `src/pages/settings/`), and the public page an
 * invite link opens (`src/components/shell/Register.tsx`). The mock's tokens and notices live
 * in the page, so a full page load starts them over: registration uses a seeded token rather
 * than one made earlier in the same test.
 */
test.describe("invite links", () => {
  test("invite by link from Users, then find the token under Settings", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.getByRole("link", { name: "Users" }).first().click();

    await page.getByRole("button", { name: "Invite by link" }).click();
    const dialog = page.getByRole("dialog", { name: "Create an invite link" });
    await expect(dialog).toBeVisible();
    await expectNoAxeViolations(page, "create invite link dialog");

    await dialog.getByRole("radio", { name: "Choose my own" }).check();
    await dialog.getByLabel(/^Custom token/).fill("e2e-invite");
    await dialog.getByRole("radio", { name: "30 days" }).check();
    await dialog.getByRole("button", { name: "Create invite link" }).click();

    const done = page.getByRole("dialog", { name: "Invite link ready" });
    await expect(done.getByText(/\/admin\/register\?token=e2e-invite$/)).toBeVisible();
    await expectNoAxeViolations(page, "invite link ready");
    await done.getByRole("button", { name: "Done" }).click();

    await page.getByRole("link", { name: "Settings" }).first().click();
    await expect(page).toHaveURL(/\/admin\/settings\/registration-tokens$/);
    const row = page.getByRole("row", { name: /e2e-invite/ });
    await expect(row.getByText("Valid")).toBeVisible();
    await expect(row.getByText("in 30 days")).toBeVisible();
    await expect(
      page.getByRole("row", { name: /spring-cohort/ }).getByText("Expired"),
    ).toBeVisible();
    await expectNoAxeViolations(page, "registration tokens");
    domGuard.assertClean();
  });

  test("an invite link registers an account, with no administrator signed in", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await page.goto("/admin/register?token=carol-invite");

    await expect(page.getByRole("heading", { name: "Create your account" })).toBeVisible();
    await expectNoAxeViolations(page, "register, empty form");

    await page.getByLabel(/^Username/).fill("alice");
    await page.getByLabel(/^Password/).fill("correct-horse-battery");
    await page.getByLabel(/^Confirm password/).fill("correct-horse-battery");
    await page.getByRole("button", { name: "Create account" }).click();
    await expect(page.getByText("That username is taken. Try another.")).toBeVisible();
    await expectNoAxeViolations(page, "register, username taken");

    await page.getByLabel(/^Username/).fill("carol");
    await page.getByRole("button", { name: "Create account" }).click();
    await expect(page.getByRole("heading", { name: "Your account is ready" })).toBeVisible();
    await expect(page.getByText("@carol:example.org")).toBeVisible();
    await expectNoAxeViolations(page, "register, done");
    domGuard.assertClean();
  });

  test("a spent invite link says so at phone width", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/admin/register?token=dave-invite");
    await expect(page.getByText(/This invite link is no longer valid/)).toBeVisible();
    await expectNoAxeViolations(page, "register, spent link, phone");
  });
});

test.describe("server notices", () => {
  test("send a notice to two users and see it in the history", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/settings/server-notices");
    await expect(page.getByRole("heading", { name: "Send a server notice" })).toBeVisible();
    await expectNoAxeViolations(page, "server notices");

    const recipients = page.getByLabel(/^Recipients/);
    await recipients.fill("@carol:elsewhere.net");
    await recipients.press("Enter");
    await expect(page.getByText(/is not on this server/)).toBeVisible();
    await expectNoAxeViolations(page, "server notices, refused recipient");

    await recipients.fill("alice");
    await recipients.press("Enter");
    await recipients.fill("spam");
    await page.getByRole("button", { name: "Add @spammer42:example.org" }).click();
    await page.getByLabel(/^Message/).fill("Maintenance tonight at 22:00.");
    await page.getByRole("button", { name: "Send to 2 users" }).click();

    await expect(page.getByText("Notice sent to 2 users.")).toBeVisible();
    await expect(page.getByText("!notices-alice:example.org")).toBeVisible();
    await expectNoAxeViolations(page, "server notices, sent");
    await expect(
      page.getByRole("row", { name: /Maintenance tonight/ }).getByText(/@alice:example.org/),
    ).toBeVisible();
    domGuard.assertClean();
  });

  test("send a notice from a user's page", async ({ page }) => {
    await signInAsOperator(page);
    await page.goto("/admin/users/%40alice%3Aexample.org");
    await page.getByRole("button", { name: "Send notice" }).click();
    const dialog = page.getByRole("dialog", { name: "Send a server notice" });
    await expect(dialog.getByText("@alice:example.org")).toBeVisible();
    await dialog.getByLabel(/^Message/).fill("Please check your email.");
    await dialog.getByRole("button", { name: "Send notice" }).click();
    await expect(dialog.getByText("Notice sent to 1 user.")).toBeVisible();
    await expectNoAxeViolations(page, "send notice dialog, sent");
    await dialog.getByRole("button", { name: "Done" }).click();
    await expect(dialog).toBeHidden();
  });
});
