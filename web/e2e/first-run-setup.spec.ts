import { test, expect, type Page } from "@playwright/test";
import { expectNoAxeViolations, installDomNestingGuard } from "./utils";

const TOKEN = "mockSetupTokenMockSetupTokenMockSetupTok";

/**
 * Opens the mock's first-run setup the way a server with no administrator has it open: a token
 * exists before the app loads (`MOCK_SETUP_TOKEN_KEY` in `src/mocks/handlers.ts`). Seeded once
 * per tab, not on every navigation, so that using the token really does close the offer.
 */
async function serverNeedsSetup(page: Page): Promise<void> {
  await page.addInitScript((token) => {
    if (sessionStorage.getItem("hs-mock:setup-seeded")) return;
    sessionStorage.setItem("hs-mock:setup-seeded", "1");
    sessionStorage.setItem("hs-mock:setup-token", token);
  }, TOKEN);
}

/**
 * First-run setup (`src/components/shell/Setup.tsx`): the page the link in a new server's log
 * opens. It is the first thing anybody ever sees of this interface, so it gets the same
 * accessibility pass as every other flow, at desktop and phone widths, in each state it has.
 */
test.describe("first-run setup", () => {
  test("the setup link leads to a signed-in administrator", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await serverNeedsSetup(page);
    await page.goto(`/admin/setup#token=${TOKEN}`);

    await expect(
      page.getByRole("heading", { name: "Create the first administrator" }),
    ).toBeVisible();
    // The link carried the token; asking for it again would be asking for nothing.
    await expect(page.getByLabel("Setup token")).toHaveCount(0);
    await expectNoAxeViolations(page, "first-run setup, empty form");

    await page.getByLabel("Username").fill("ops");
    await page.getByLabel(/^Password/).fill("short");
    await page.getByLabel("Confirm password").fill("short");
    await page.getByRole("button", { name: "Create administrator" }).click();
    await expect(page.getByText(/Password too short/)).toBeVisible();
    await expectNoAxeViolations(page, "first-run setup, refused password");

    await page.getByLabel(/^Password/).fill("hunter2-ops");
    await page.getByLabel("Confirm password").fill("hunter2-ops");
    await page.getByRole("button", { name: "Create administrator" }).click();

    await expect(page.getByRole("heading", { name: "Overview" })).toBeVisible();
    // The token does not stay in the address bar, or in the history behind it.
    expect(page.url()).not.toContain(TOKEN);
    expect(page.url()).not.toContain("/setup");
    domGuard.assertClean();
  });

  test("without the link it asks for the token, and reads at phone width", async ({ page }) => {
    await page.setViewportSize({ width: 390, height: 844 });
    await serverNeedsSetup(page);
    await page.goto("/admin/setup");

    await expect(page.getByLabel("Setup token")).toBeVisible();
    await expectNoAxeViolations(page, "first-run setup, phone width, token field");

    await page.getByLabel("Setup token").fill("not-the-token");
    await page.getByLabel("Username").fill("ops");
    await page.getByLabel(/^Password/).fill("hunter2-ops");
    await page.getByLabel("Confirm password").fill("hunter2-ops");
    await page.getByRole("button", { name: "Create administrator" }).click();
    await expect(page.getByText(/isn't this server's setup token/)).toBeVisible();
    await expectNoAxeViolations(page, "first-run setup, phone width, wrong token");
  });

  test("a server that is already set up says so instead of showing a form", async ({ page }) => {
    await page.goto(`/admin/setup#token=${TOKEN}`);

    await expect(
      page.getByRole("heading", { name: "This server is already set up" }),
    ).toBeVisible();
    await expect(page.getByRole("button", { name: "Create administrator" })).toHaveCount(0);
    await expectNoAxeViolations(page, "first-run setup, already set up");

    await page.getByRole("button", { name: "Go to sign in" }).click();
    await expect(page.getByRole("button", { name: "Sign in as operator" })).toBeVisible();
  });
});
