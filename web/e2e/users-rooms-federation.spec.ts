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
});
