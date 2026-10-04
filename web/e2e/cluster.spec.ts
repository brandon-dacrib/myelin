import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

// The Cluster page on the mock's three replicas: drain one from its row, watch its shards move to
// the others until it is drained, open the drain's task, then undrain it and see it take its
// share back.

test.describe("Cluster", () => {
  test("drain a replica, follow it to drained, open its task, undrain it", async ({ page }) => {
    test.setTimeout(60_000);
    const guard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page
      .getByRole("navigation", { name: "Primary" })
      .getByRole("link", { name: "Cluster" })
      .click();
    await expect(page.getByRole("heading", { name: "Cluster", level: 1 })).toBeVisible();

    const replicas = page.getByRole("table", { name: "Replicas" });
    const hs0 = replicas.getByRole("row").filter({ hasText: "hs-0" });
    const hs1 = replicas.getByRole("row").filter({ hasText: "hs-1" });
    await expect(hs0).toContainText("This replica");
    await expect(hs1).toContainText("Active");
    const hs1Before = Number(await hs1.getByRole("cell").nth(2).textContent());
    expect(hs1Before).toBeGreaterThan(0);
    await expect(page.getByText("Every shard has an owner")).toBeVisible();
    await expect(page.getByRole("img", { name: /^64 room shards: / })).toBeVisible();
    await expectNoAxeViolations(page, "cluster");

    await hs1.getByRole("button", { name: "Drain hs-1" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.getByRole("heading", { name: "Drain hs-1?" })).toBeVisible();
    await expect(dialog).toContainText("survives a restart");
    await expectNoAxeViolations(page, "drain dialog");
    await dialog.getByRole("button", { name: "Drain replica" }).click();
    await expect(dialog).toBeHidden();

    await expect(hs1).toContainText("Draining");
    await expect(hs1.getByRole("progressbar")).toBeVisible();
    await expect(hs1).toContainText("@admin:example.org");
    await expect(hs1).toContainText("Drained", { timeout: 15_000 });
    await expect(hs1.getByRole("cell").nth(2)).toHaveText("0");
    await expect(page.getByText("Every shard has an owner")).toBeVisible();
    await expect(page.getByRole("img", { name: /hs-1 owns/ })).toHaveCount(0);
    await expectNoAxeViolations(page, "cluster, one replica drained");

    // The drain's task, from the row, on the Tasks page.
    await hs1.getByRole("link", { name: "Drain task for hs-1" }).click();
    await expect(page.getByRole("heading", { name: "Drain replica" })).toBeVisible();
    await expect(page.getByText("Succeeded")).toBeVisible();
    await page.goBack();

    await hs1.getByRole("button", { name: "Undrain hs-1" }).click();
    await expect(dialog.getByRole("heading", { name: "Undrain hs-1?" })).toBeVisible();
    await dialog.getByRole("button", { name: "Undrain replica" }).click();
    await expect(hs1).toContainText("Active");
    await expect(hs1.getByRole("cell").nth(2)).toHaveText(String(hs1Before));
    await expect(hs1.getByRole("link", { name: /Drain task/ })).toHaveCount(0);
    guard.assertClean();
  });

  test("each replica's heartbeat sequence, and whether its heartbeats still arrive", async ({
    page,
  }) => {
    test.setTimeout(60_000);
    await signInAsOperator(page);
    await page
      .getByRole("navigation", { name: "Primary" })
      .getByRole("link", { name: "Cluster" })
      .click();
    const hs0 = page
      .getByRole("table", { name: "Replicas" })
      .getByRole("row")
      .filter({ hasText: "hs-0" });
    await expect(hs0).toContainText(/seq [\d,]+/);
    await expect(page.getByText("Heartbeat", { exact: true })).toBeVisible();
    await expect(page.getByText("Drains released at once", { exact: true })).toBeVisible();
    await expect(page.getByText("Since this replica started")).toBeVisible();
    // The next poll (15 s) reads a higher number: the mock heartbeats every two seconds.
    await expect(hs0).toContainText(/\+\d+ since the last poll/, { timeout: 30_000 });
    await expect(page.getByText("This replica, still arriving")).toBeVisible();
    await expectNoAxeViolations(page, "cluster heartbeats");
  });

  test("the shard table filters by kind and pages, and the page works at phone width", async ({
    page,
  }) => {
    await signInAsOperator(page);
    await page.goto("/admin/cluster");
    await page.getByRole("combobox", { name: "Kind" }).click();
    await page.getByRole("option", { name: "Rooms" }).click();
    await expect(page).toHaveURL(/kind=room/);
    await page.getByRole("button", { name: "Table" }).click();
    await expect(page).toHaveURL(/view=table/);
    const shards = page.getByRole("table", { name: "Shards" });
    await expect(shards).toContainText("room/0");
    await expect(shards.getByRole("row")).toHaveCount(51);
    await page.getByRole("button", { name: "Next" }).click();
    await expect(shards).toContainText("room/50");
    await expect(shards).not.toContainText("user/");

    await page.setViewportSize({ width: 390, height: 844 });
    await page.goto("/admin/cluster");
    await expect(page.getByRole("button", { name: "Drain hs-1" })).toBeVisible();
    await expectNoAxeViolations(page, "cluster at phone width");
    const overflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
    );
    expect(overflow).toBeLessThanOrEqual(0);
  });

  test("a 503 from the replicas says it isn't connected, not an error", async ({ page }) => {
    await signInAsOperator(page);
    await page.evaluate(() =>
      window.__hsAdminMock!.setForceProblem("/api/v1/cluster/replicas", 503, {
        detail: "The cluster registry is not attached.",
      }),
    );
    await page.goto("/admin/cluster");
    await expect(
      page.getByText("Replicas isn't connected to a data source on this server yet"),
    ).toBeVisible();
    await expect(page.getByText("The cluster registry is not attached.")).toBeVisible();
    await expect(page.getByRole("table", { name: "Replicas" })).toHaveCount(0);
    await expect(page.getByRole("alert")).toHaveCount(0);
  });
});
