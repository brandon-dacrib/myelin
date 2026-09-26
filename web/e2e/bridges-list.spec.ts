import { test, expect, devices } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

// Regression coverage for the nested-<button> defect the integration
// review found on the bridges list (DataTable's <768px card fallback
// nested the "actions" column's real <button>s, and the "name" column's
// <a>, inside its own tap-target <button> — invalid HTML that axe's
// default desktop-viewport run could not see, because that markup is
// `display:none` above 768px). Two independent checks, deliberately not
// relying on each other:
//
//  1. installDomNestingGuard: catches React's own "invalid DOM nesting"
//     console warning, which fires regardless of viewport (it validates the
//     actual DOM tree, not what's currently visible).
//  2. An axe pass at a phone viewport, where the card fallback is actually
//     on screen: axe's "nested-interactive" rule covers this once the
//     markup is visible, so this closes the gap for anyone who trusts axe
//     alone and never resizes below 768px.

test.describe("Bridges list", () => {
  test("desktop: no invalid DOM nesting", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.getByRole("link", { name: "Bridges" }).first().click();
    await expect(page.getByRole("heading", { name: "Bridges" })).toBeVisible();
    await expect(page.getByRole("table")).toBeVisible();

    // Attention first: the one that is down leads, the paused one trails.
    const names = page.getByRole("table").getByRole("link");
    await expect(names.first()).toHaveText("Signal");
    await expect(names.last()).toHaveText("Discord");
    // The summary strip counts each state and filters the table.
    await page.getByRole("button", { name: /^Down 1$/ }).click();
    await expect(page).toHaveURL(/state=down/);
    await expect(names).toHaveCount(1);
    await page.getByRole("button", { name: /^All 5$/ }).click();
    await expect(names).toHaveCount(5);
    await expectNoAxeViolations(page, "bridges list");
    domGuard.assertClean();
  });

  test("phone viewport: card fallback is accessible and has no nested controls", async ({
    page,
  }) => {
    await page.setViewportSize(devices["iPhone 13"].viewport);
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    // Below 1024px the sidebar is a drawer, not an inline link (AppShell).
    await page.getByRole("button", { name: "Open navigation" }).click();
    await page.getByRole("link", { name: "Bridges" }).first().click();
    await expect(page.getByRole("heading", { name: "Bridges" })).toBeVisible();

    // The card fallback is what's on screen at this width, not the table.
    // (Scoped past `getByRole("list")`: the toast viewport is also an <ol>.)
    const cardList = page.locator("ul.divide-y.divide-border");
    await expect(page.getByRole("table")).toBeHidden();
    await expect(cardList).toBeVisible();

    // Every action button must be reachable and distinct from the card's
    // own tap target, not swallowed by it.
    const pauseOrResume = page.getByRole("button", { name: /^(Pause|Resume) /i });
    await expect(pauseOrResume.first()).toBeVisible();

    await expectNoAxeViolations(page, "bridges list (phone viewport)");
    domGuard.assertClean();
  });
});
