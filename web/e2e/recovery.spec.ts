import { test, expect } from "@playwright/test";
import { expectNoAxeViolations, installDomNestingGuard } from "./utils";

const TOKEN = "mock-recovery-token";

/**
 * Administrator recovery (`src/components/shell/Recover.tsx`): the page the link from
 * `hs recover` opens when nobody can sign in. The mock's link is open by default and works once
 * per tab (`src/mocks/data/recovery.ts`). Like its sibling, first-run setup, it gets the same
 * accessibility pass as every other flow, at desktop and phone widths, in each state it has.
 */
test.describe("administrator recovery", () => {
  test("the recovery link leads to a signed-in administrator", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await page.goto(`/admin/recover#token=${TOKEN}`);

    await expect(page.getByRole("heading", { name: "Recover administrator access" })).toBeVisible();
    await expect(page.getByText(/This link works once and expires in 1[45] minutes/)).toBeVisible();
    await expect(page.getByRole("radio", { name: "@admin:example.org" })).toBeVisible();
    await expectNoAxeViolations(page, "recovery, empty form");

    await page.getByRole("radio", { name: "@ops:example.org" }).check();
    await page.getByLabel("New password").fill("short");
    await page.getByLabel("Confirm password").fill("short");
    await page.getByRole("button", { name: "Reset password and sign in" }).click();
    await expect(page.getByText(/Password too short/)).toBeVisible();
    await expectNoAxeViolations(page, "recovery, refused password");

    await page.getByLabel("New password").fill("hunter2-ops");
    await page.getByLabel("Confirm password").fill("hunter2-ops");
    await page.getByRole("button", { name: "Reset password and sign in" }).click();

    await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible();
    // The token does not stay in the address bar, or in the history behind it.
    expect(page.url()).not.toContain(TOKEN);
    expect(page.url()).not.toContain("/recover");
    domGuard.assertClean();
  });

  test("a wrong token, and a missing one, are explained at phone width", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/admin/recover#token=not-the-token");

    await expect(page.getByText(/This is not this server's recovery link/)).toBeVisible();
    await expect(page.getByRole("button", { name: "Reset password and sign in" })).toHaveCount(0);
    await expectNoAxeViolations(page, "recovery, phone width, wrong token");

    await page.goto("/admin/recover");
    await expect(page.getByText(/It needs the link that/)).toBeVisible();
    await expectNoAxeViolations(page, "recovery, phone width, no token");

    await page.getByRole("button", { name: "Go to sign in" }).click();
    await expect(page.getByRole("button", { name: "Sign in as operator" })).toBeVisible();
    await expect(page.getByText(/Locked out\? Run hs recover/)).toBeVisible();
  });

  test("a used link says no link is open", async ({ page }) => {
    await page.addInitScript(() => sessionStorage.setItem("hs-mock:recovery-used", "1"));
    await page.goto(`/admin/recover#token=${TOKEN}`);

    await expect(page.getByText(/No recovery link is open/)).toBeVisible();
    await expect(page.getByRole("button", { name: "Reset password and sign in" })).toHaveCount(0);
    await expectNoAxeViolations(page, "recovery, used link");
  });
});
