import { test, expect, type Page } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

// The room page's long tail on the mock: state, the timeline and jumping to a date, aliases,
// forward extremities, media, joining a user, purging history and deleting the room -- the last
// three as tasks the page follows to the end.

async function openGeneral(page: Page) {
  await signInAsOperator(page);
  await page.getByRole("link", { name: "Rooms" }).first().click();
  await page.getByRole("link", { name: "General" }).click();
  await expect(page.getByRole("heading", { name: "General", level: 1 })).toBeVisible();
}

test.describe("Room page", () => {
  test("browse state and timeline, jump to a date, add and remove an alias", async ({ page }) => {
    const guard = installDomNestingGuard(page);
    await openGeneral(page);
    await expectNoAxeViolations(page, "room overview");

    await page.getByRole("tab", { name: "State" }).click();
    const state = page.getByRole("table", { name: "Room state" });
    await expect(state.getByText("m.room.create")).toBeVisible();
    await expectNoAxeViolations(page, "room state");

    await page.getByRole("tab", { name: "Timeline" }).click();
    const messages = page.getByRole("list", { name: "Messages" });
    await expect(messages.getByText("Message 30 in General")).toBeVisible();
    await page.getByRole("button", { name: "Load older" }).click();
    await expect(messages.getByText("Message 1 in General")).toBeVisible();
    await page.getByLabel("Jump to date").fill("1970-01-02T00:00");
    await page.getByRole("button", { name: "Jump", exact: true }).click();
    await expect(page.getByRole("complementary", { name: "Event in context" })).toBeVisible();
    await expectNoAxeViolations(page, "room timeline");

    await page.getByRole("tab", { name: "Aliases" }).click();
    const aliases = page.getByRole("list", { name: "Aliases" });
    await page.getByLabel("New alias").fill("#chat:example.org");
    await page.getByRole("button", { name: "Add alias" }).click();
    await expect(aliases.getByText("#chat:example.org")).toBeVisible();
    await page.getByRole("button", { name: "Remove #chat:example.org" }).click();
    await page.getByRole("button", { name: "Remove alias" }).click();
    await expect(aliases.getByText("#chat:example.org")).toHaveCount(0);
    guard.assertClean();
  });

  test("prune extremities, quarantine media, purge history", async ({ page }) => {
    const guard = installDomNestingGuard(page);
    await openGeneral(page);

    await page.getByRole("tab", { name: "Extremities" }).click();
    const extremities = page.getByRole("table", { name: "Forward extremities" });
    await expect(extremities.getByRole("row")).toHaveCount(3);
    await page.getByRole("button", { name: "Prune to one" }).click();
    await expect(extremities.getByRole("row")).toHaveCount(2);

    await page.getByRole("tab", { name: "Media" }).click();
    await page.getByRole("button", { name: "Quarantine all" }).click();
    await page.getByRole("button", { name: "Quarantine media" }).click();
    await expect(page.getByText(/^Quarantined 2 items/)).toBeVisible({ timeout: 10_000 });

    await page.getByRole("button", { name: "Purge history" }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel("Purge messages sent before").fill("2999-01-01T00:00");
    await dialog.getByRole("checkbox", { name: /own users/ }).check();
    await expectNoAxeViolations(page, "purge dialog");
    await dialog.getByRole("button", { name: "Purge history" }).click();
    await expect(dialog.getByText(/^Purged 30 events/)).toBeVisible({ timeout: 10_000 });
    await dialog.getByRole("button", { name: "Close" }).first().click();

    await page.getByRole("tab", { name: "Timeline" }).click();
    await expect(page.getByText("Message 30 in General")).toHaveCount(0);
    guard.assertClean();
  });

  test("join a user, then delete the room and land on the rooms list", async ({ page }) => {
    const guard = installDomNestingGuard(page);
    await openGeneral(page);

    await page.getByRole("button", { name: "Join a user" }).click();
    await page.getByLabel("User ID").fill("@bob:example.org");
    await page.getByRole("button", { name: "Join user" }).click();
    await expect(page.getByText("@bob:example.org", { exact: true })).toBeVisible();

    await page.getByRole("button", { name: "Delete room" }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByRole("checkbox", { name: /Block it/ }).check();
    await dialog.getByLabel(/to confirm/).fill("General");
    await expectNoAxeViolations(page, "delete dialog");
    await dialog.getByRole("button", { name: "Delete room" }).click();
    await expect(page.getByRole("heading", { name: "Rooms", level: 1 })).toBeVisible({
      timeout: 10_000,
    });
    await expect(page.getByRole("link", { name: "General" })).toHaveCount(0);
    guard.assertClean();
  });

  test("a space shows its rooms", async ({ page }) => {
    await signInAsOperator(page);
    await page.getByRole("link", { name: "Rooms" }).first().click();
    await page.getByRole("link", { name: "Engineering" }).click();
    await page.getByRole("tab", { name: "Space" }).click();
    const hierarchy = page.getByRole("list", { name: "Hierarchy" });
    await expect(hierarchy.getByRole("link", { name: "General" })).toBeVisible();
    await expect(hierarchy.getByText("Not on this server")).toBeVisible();
    await expectNoAxeViolations(page, "space hierarchy");
  });
});
