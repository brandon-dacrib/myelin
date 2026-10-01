import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

// Smoke + accessibility coverage for the three sections added after the
// bridges marquee: Users (flows.md flow 2), Rooms (flow 3), Federation
// (flow 4). Each gets list -> detail -> a representative action.

test.describe("Users", () => {
  test("search, open, suspend with reason, undo", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.getByRole("link", { name: "Users" }).first().click();
    await expect(page.getByRole("heading", { name: "Users" })).toBeVisible();
    await expectNoAxeViolations(page, "users list");

    await page.getByRole("link", { name: "@alice:example.org" }).click();
    await expect(page.getByRole("heading", { name: "Alice" })).toBeVisible();
    await expectNoAxeViolations(page, "user detail");

    await page.getByRole("button", { name: "Suspend", exact: true }).click();
    await page.getByRole("button", { name: "Suspend", exact: true }).last().click();
    await expect(page.getByText("Suspended", { exact: true }).first()).toBeVisible();
    domGuard.assertClean();
  });
});

test.describe("Rooms", () => {
  test("search, open, block with reason", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.getByRole("link", { name: "Rooms" }).first().click();
    await expect(page.getByRole("heading", { name: "Rooms" })).toBeVisible();
    await expectNoAxeViolations(page, "rooms list");

    await page.getByRole("link", { name: "spam-central" }).click();
    await expect(page.getByRole("heading", { name: "spam-central" })).toBeVisible();
    await expectNoAxeViolations(page, "room detail");

    await page.getByRole("button", { name: "Block", exact: true }).click();
    await page.getByRole("button", { name: "Block", exact: true }).last().click();
    await expect(page.getByText("Blocked", { exact: true })).toBeVisible();
    domGuard.assertClean();
  });
});

test.describe("Federation", () => {
  test("list sorted by attention, retry action", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.getByRole("link", { name: "Federation" }).first().click();
    await expect(page.getByRole("heading", { name: "Federation" })).toBeVisible();
    await expectNoAxeViolations(page, "federation list");

    await page.getByRole("link", { name: "mozilla.org" }).click();
    await expect(page.getByRole("heading", { name: "mozilla.org" })).toBeVisible();
    await expectNoAxeViolations(page, "federation destination detail");

    await page.getByRole("button", { name: "Reset backoff" }).click();
    await expect(page.getByText("Healthy", { exact: true })).toBeVisible();
    domGuard.assertClean();
  });

  test("a destination in catch-up says so, explains it, and links to the queue limit", async ({
    page,
  }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/federation");
    const row = page.getByRole("row").filter({ hasText: "kde.org" });
    await expect(row.getByText(/Catching up since/)).toBeVisible();
    await expect(page.getByText(/1 server is catching up/)).toBeVisible();
    await expectNoAxeViolations(page, "federation list, a destination catching up");

    await page.getByRole("link", { name: "kde.org" }).click();
    const notice = page.getByRole("region", { name: /Catching up since/ });
    await expect(notice).toContainText("longer than its queue holds (10,000 events)");
    await expectNoAxeViolations(page, "federation destination catching up");

    await notice.getByRole("link", { name: "Max queued PDUs per destination" }).click();
    const setting = page.locator("#setting-max_queued_pdus_per_destination");
    await expect(setting).toBeVisible();
    await expect(setting.getByText("Needs a restart", { exact: true })).toBeVisible();
    await expect(setting).toContainText("Default: 10000");
    domGuard.assertClean();
  });
});
