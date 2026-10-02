import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations } from "./utils";

/** A room's lifecycle on its page: guest access, an upgrade's successor, and the block reason. */
test.describe("room lifecycle", () => {
  test("block with a reason shows it; an upgraded room links where people went", async ({
    page,
  }) => {
    await signInAsOperator(page);
    await page.goto("/admin/rooms/!spam-central:example.org");
    await expect(page.getByRole("heading", { name: "spam-central", level: 1 })).toBeVisible();
    await expect(page.getByText("Guests may join")).toBeVisible();

    await page.getByRole("button", { name: "Block", exact: true }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.getByText(/Nobody can join it any more/)).toBeVisible();
    await expectNoAxeViolations(page, "block room dialog");
    await dialog.getByLabel(/^Reason/).fill("Spam ring");
    await dialog.getByRole("button", { name: "Block" }).click();
    await expect(page.getByText("Blocked: Spam ring")).toBeVisible();
    await expect(page.getByText("Blocked because")).toBeVisible();
    await expectNoAxeViolations(page, "blocked room with a reason");

    await page.goto("/admin/rooms/!general-v6:example.org");
    await expect(page.getByText("Upgraded", { exact: true })).toBeVisible();
    await expect(page.getByText(/This room was upgraded and closed/)).toBeVisible();
    await page.getByRole("link", { name: "!general:example.org" }).first().click();
    await expect(page.getByRole("heading", { name: "General", level: 1 })).toBeVisible();
  });
});
