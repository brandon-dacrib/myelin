import { test, expect } from "@playwright/test";
import {
  signInAsOperator,
  signInReadOnly,
  expectNoAxeViolations,
  installDomNestingGuard,
} from "./utils";

// The moderation queue, long-running tasks and statistics, each reached the way an operator
// would: from the Overview's attention list or its activity strip.

test.describe("Reports", () => {
  test("from the Overview to a decision, and the counts follow", async ({ page }) => {
    const guard = installDomNestingGuard(page);
    await signInAsOperator(page);
    const primary = page.getByRole("navigation", { name: "Primary" });
    await expect(primary.getByRole("link", { name: /Reports/ })).toContainText("4");

    await expect(page.getByText("4 reports awaiting action.")).toBeVisible();
    await page.getByRole("button", { name: "Open reports" }).click();
    await expect(page.getByRole("heading", { name: "Reports", level: 1 })).toBeVisible();
    await expect(page.getByRole("table", { name: "Reports" }).getByRole("row")).toHaveCount(5);
    await expectNoAxeViolations(page, "reports queue");

    await page
      .getByRole("table", { name: "Reports" })
      .getByRole("link", { name: "Message in !general:example.org" })
      .first()
      .click();
    await expect(page.getByText(/Free crypto!!!/)).toBeVisible();
    await expectNoAxeViolations(page, "report detail");

    await page.getByRole("radio", { name: /Redacted the content/ }).check();
    await page.getByLabel("Note", { exact: true }).fill("Redacted and warned.");
    await page.getByRole("button", { name: "Resolve report" }).click();
    await expect(page.getByRole("heading", { name: "Decision" })).toBeVisible();
    await expect(page.getByText("Redacted and warned.")).toBeVisible();
    await expectNoAxeViolations(page, "resolved report");

    await page.getByRole("link", { name: "Reports", exact: true }).first().click();
    await expect(page.getByRole("table", { name: "Reports" }).getByRole("row")).toHaveCount(4);
    await expect(primary.getByRole("link", { name: /Reports/ })).toContainText("3");

    // Every report about the room, closed ones included, from the report's room link.
    await page.goto("/admin/reports?status=all&room_id=!general:example.org");
    await expect(page.getByRole("table", { name: "Reports" }).getByRole("row")).toHaveCount(3);
    await expect(page.getByText("Only reports about")).toBeVisible();
    guard.assertClean();
  });

  test("filters live in the URL and survive going back", async ({ page }) => {
    await signInAsOperator(page);
    await page.goto("/admin/reports");
    await page.getByRole("combobox", { name: "Kind" }).click();
    await page.getByRole("option", { name: "Users" }).click();
    await expect(page).toHaveURL(/kind=user/);
    await expect(page.getByRole("table", { name: "Reports" }).getByRole("row")).toHaveCount(2);
    await page.getByRole("combobox", { name: "Status" }).click();
    await page.getByRole("option", { name: "Dismissed" }).click();
    await expect(page).toHaveURL(/status=dismissed/);
    await expect(page.getByText("She reported me").first()).toBeVisible();
    await page.goBack();
    await expect(page.getByText("Sends me unsolicited invites every hour").first()).toBeVisible();
  });
});

test.describe("Tasks", () => {
  test("a failed task from the Overview, then a running one cancelled", async ({ page }) => {
    const guard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await expect(page.getByText(/Task "Delete room" failed/)).toBeVisible();
    await page.getByRole("button", { name: "Open task" }).click();
    await expect(page.getByRole("heading", { name: "Delete room" })).toBeVisible();
    await expect(page.getByRole("alert")).toContainText("owner replica stopped answering");
    await expectNoAxeViolations(page, "failed task");

    await page.getByRole("link", { name: "Tasks", exact: true }).first().click();
    await expect(page.getByRole("heading", { name: "Tasks", level: 1 })).toBeVisible();
    await expectNoAxeViolations(page, "tasks list");

    await page
      .getByRole("table", { name: "Tasks" })
      .getByRole("link", { name: "Purge remote media cache" })
      .click();
    const bar = page.getByRole("progressbar");
    const first = Number(await bar.getAttribute("aria-valuenow"));
    // The page polls a running task; the mock's purge moves on a clock.
    await expect
      .poll(async () => Number(await bar.getAttribute("aria-valuenow")), { timeout: 10_000 })
      .toBeGreaterThan(first);
    await expectNoAxeViolations(page, "running task");

    await page.getByRole("button", { name: "Cancel task" }).click();
    await expect(page.getByRole("dialog")).toContainText("nothing is rolled back");
    await expectNoAxeViolations(page, "cancel dialog");
    await page.getByRole("dialog").getByRole("button", { name: "Cancel task" }).click();
    await expect(page.getByText("Cancelled", { exact: true }).first()).toBeVisible();
    await expect(page.getByRole("button", { name: "Cancel task" })).toHaveCount(0);
    guard.assertClean();
  });

  test("read-only operators see tasks but cannot cancel them", async ({ page }) => {
    await signInReadOnly(page);
    await page.goto("/admin/tasks/01J9ZT000000000000000000T5");
    await expect(page.getByRole("heading", { name: "Purge room history" })).toBeVisible();
    await expect(page.getByText("Runs at")).toBeVisible();
    await expect(page.getByRole("button", { name: "Cancel task" })).toHaveCount(0);
  });
});

test.describe("Statistics", () => {
  test("from the Overview's activity to charts, a range, a tooltip and sorted tables", async ({
    page,
  }) => {
    const guard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await expect(page.getByRole("heading", { name: "Activity" })).toBeVisible();
    await page.getByRole("link", { name: /New accounts, 7 days/ }).click();
    await expect(page.getByRole("heading", { name: "Statistics", level: 1 })).toBeVisible();
    await expect(
      page.getByRole("img", { name: /New accounts, last 7 days: \d+ points/ }),
    ).toBeVisible();
    await expectNoAxeViolations(page, "statistics");

    await page.getByRole("combobox", { name: "Range" }).click();
    await page.getByRole("option", { name: "Last 30 days" }).click();
    await expect(page).toHaveURL(/range=30d/);
    const chart = page.getByRole("img", { name: /New accounts, last 30 days: \d+ points/ });
    await expect(chart).toBeVisible();

    await chart.scrollIntoViewIfNeeded();
    const box = (await chart.boundingBox())!;
    await page.mouse.move(box.x + box.width * 0.5, box.y + box.height * 0.5);
    await expect(page.getByRole("tooltip")).toBeVisible();

    const reportsCard = page.getByRole("region", { name: "Reports received" });
    await reportsCard.getByRole("button", { name: "Show as table" }).click();
    await expect(reportsCard.getByRole("table")).toBeVisible();
    await expectNoAxeViolations(page, "statistics with a table open");

    const rooms = page.getByRole("table", { name: "Largest rooms" });
    await expect(rooms.getByRole("row").nth(1)).toContainText("Announcements");
    await rooms.getByRole("button", { name: /State events/ }).click();
    await expect(page).toHaveURL(/rooms_sort=state_events_count/);
    await rooms.getByRole("button", { name: /State events/ }).click();
    await expect(page).toHaveURL(/rooms_sort=-state_events_count/);
    await expect(rooms.getByRole("row").nth(1)).toContainText("Support");
    guard.assertClean();
  });
});
