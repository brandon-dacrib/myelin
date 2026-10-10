import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

// Forgetting federation destinations (decision 0042): the servers sharing no room are listed
// on their own, one is forgotten from its row after a dialog that says what goes, the prune
// panel previews before forgetting the rest, and the sweep's setting is one link away.

test.describe("Forgetting federation destinations", () => {
  test("filters to the servers sharing no room, forgets one, then prunes the rest after a preview", async ({
    page,
  }) => {
    const guard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/federation");
    const table = page.getByRole("table", { name: "Federation destinations" });
    await expect(table.getByRole("row")).toHaveCount(51);

    await page.getByRole("button", { name: "No shared room" }).click();
    await expect(page).toHaveURL(/show=no-shared-room$/);
    await expect(table.getByRole("row")).toHaveCount(9);
    await expect(page.getByText("8 servers sharing no room")).toBeVisible();
    const row = table.getByRole("row").filter({ hasText: "srv-07.example.net" });
    await expect(row.getByText("none")).toBeVisible();
    await expectNoAxeViolations(page, "federation, no shared room");

    await row.getByRole("button", { name: "Forget srv-07.example.net" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toContainText("No room is shared with it");
    await expect(dialog.getByRole("switch")).toHaveCount(0);
    await expectNoAxeViolations(page, "forget destination dialog");
    await dialog.getByRole("button", { name: "Forget" }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByRole("status")).toContainText("Forgot srv-07.example.net");
    await expect(table.getByRole("row")).toHaveCount(8);
    await expect(page.getByText("7 servers sharing no room")).toBeVisible();

    const panel = page.getByRole("region", { name: "Servers this one shares no room with" });
    await expect(panel).toContainText("nothing happen for 1 week");
    await panel.getByRole("button", { name: "Preview prune" }).click();
    const preview = page.getByRole("region", { name: "If you prune now" });
    await expect(preview).toContainText("7 servers would be forgotten; 57 kept.");
    await expect(preview).toContainText("7: Shares no room, nothing queued");
    await expect(preview).toContainText("57: Shares a room");
    await expectNoAxeViolations(page, "prune preview");
    await preview.getByRole("button", { name: "Forget 7 servers" }).click();
    await expect(page.getByRole("region", { name: "Pruned" })).toContainText(
      "7 servers were forgotten; 57 kept.",
    );
    await expect(page.getByText("Every known server shares a room")).toBeVisible();

    await page.getByRole("button", { name: "Every server" }).click();
    await expect(page.getByText("57 servers", { exact: true })).toBeVisible();

    await panel.getByRole("link", { name: "Forget unused destinations after" }).click();
    const setting = page.locator("#setting-forget_unused_destinations_after");
    await expect(setting).toBeVisible();
    await expect(setting.getByText("Applies on save", { exact: true })).toBeVisible();
    await expect(setting).toContainText("Default: 1w");
    guard.assertClean();
  });

  test("a server sharing rooms is forgotten only when the operator insists, from its own page", async ({
    page,
  }) => {
    const guard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/federation/mozilla.org");
    await expect(page.getByRole("heading", { name: "mozilla.org" })).toBeVisible();
    await expect(page.locator("dt").filter({ hasText: /^Shared rooms$/ })).toBeVisible();
    await page.getByRole("button", { name: "Forget this server" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog).toContainText("still shares 2 rooms with it");
    await expect(dialog.getByRole("button", { name: "Forget" })).toBeDisabled();
    await expectNoAxeViolations(page, "forget destination dialog, rooms shared");
    await dialog.getByRole("switch", { name: "Forget it anyway" }).click();
    await dialog.getByRole("button", { name: "Forget" }).click();
    await expect(page).toHaveURL(/\/admin\/federation$/);
    await expect(page.getByRole("status")).toContainText("Forgot mozilla.org");
    await expect(page.getByRole("status")).toContainText("Dropped 42 events, 3 messages.");
    await expect(
      page.getByRole("table", { name: "Federation destinations" }).getByRole("row"),
    ).toHaveCount(51);
    await expect(page.getByText("64 servers", { exact: true })).toBeVisible();

    await page.setViewportSize({ width: 390, height: 844 });
    await expectNoAxeViolations(page, "federation with prune panel at phone width");
    guard.assertClean();
  });
});
