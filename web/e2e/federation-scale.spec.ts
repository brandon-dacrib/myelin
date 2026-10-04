import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

// Federation at scale on the mock's 65 destinations: the Overview counts failing servers from the
// server's own total, not from a page; the Federation page filters, sorts and pages through the
// server, with the filter, sort and cursor in the URL.

test.describe("Federation at scale", () => {
  test("the Overview's counts are the server's, and open the list filtered", async ({ page }) => {
    const guard = installDomNestingGuard(page);
    await signInAsOperator(page);
    const strip = page.getByRole("region", { name: "Federation" });
    await expect(strip.getByRole("button", { name: /^2 Failing$/ })).toBeVisible();
    await expect(strip.getByRole("button", { name: /^63 Not failing$/ })).toBeVisible();
    await expect(strip).toContainText("counted by the server");
    await expectNoAxeViolations(page, "overview federation strip");

    await strip.getByRole("button", { name: /^2 Failing$/ }).click();
    await expect(page).toHaveURL(/\/admin\/federation\?show=failing$/);
    const table = page.getByRole("table", { name: "Federation destinations" });
    await expect(table.getByRole("row")).toHaveCount(3);
    await expect(table.getByRole("row").nth(1)).toContainText("kde.org");
    await expect(page.getByText("2 servers failing")).toBeVisible();
    await expect(page.getByRole("button", { name: "Failing", exact: true })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    await expectNoAxeViolations(page, "federation, failing only");
    guard.assertClean();
  });

  test("the list pages, filters and sorts through the server, in the URL", async ({ page }) => {
    await signInAsOperator(page);
    await page
      .getByRole("navigation", { name: "Primary" })
      .getByRole("link", { name: "Federation" })
      .click();
    const table = page.getByRole("table", { name: "Federation destinations" });
    await expect(table.getByRole("row")).toHaveCount(51);
    await expect(page.getByText("65 servers")).toBeVisible();
    await expect(page.getByText("Failing servers first, then the rest by name.")).toBeVisible();
    await expect(table.getByRole("row").nth(1)).toContainText("kde.org");
    await expect(table.getByRole("row").nth(3)).toContainText("element.io");

    await page.getByRole("button", { name: "Next" }).click();
    await expect(page).toHaveURL(/cursor=/);
    await expect(table.getByRole("row")).toHaveCount(16);
    await expect(table).toContainText("srv-60.example.net");
    await expect(page.getByRole("button", { name: "Next" })).toBeDisabled();
    await page.getByRole("button", { name: "Previous" }).click();
    await expect(table.getByRole("row")).toHaveCount(51);

    await page.getByRole("button", { name: "Not failing" }).click();
    await expect(page).toHaveURL(/show=not-failing/);
    await expect(page.getByText("63 servers not failing")).toBeVisible();
    await expect(table).not.toContainText("kde.org");

    await page.getByRole("button", { name: "Every server" }).click();
    await page
      .getByRole("columnheader", { name: /Last success/ })
      .getByRole("button")
      .click();
    await expect(page).toHaveURL(/sort=last_successful_at/);
    await expect(table.getByRole("row").nth(1)).toContainText("kde.org");
    await expect(page.getByText(/Sorted by the column you chose/)).toBeVisible();
    await page
      .getByRole("columnheader", { name: /Last success/ })
      .getByRole("button")
      .click();
    await expect(page).toHaveURL(/sort=-last_successful_at/);
    await expect(table.getByRole("row").nth(1)).toContainText("matrix.org");
    await expectNoAxeViolations(page, "federation, sorted");

    // The filter and sort survive a reload: they are the URL.
    await page.reload();
    await expect(table.getByRole("row").nth(1)).toContainText("matrix.org");

    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.getByRole("group", { name: "Show" })).toBeVisible();
    await expectNoAxeViolations(page, "federation at phone width");
  });
});
