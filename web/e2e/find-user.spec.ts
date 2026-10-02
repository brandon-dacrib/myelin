import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations } from "./utils";

/** The exact lookup on the Users page (`src/pages/users/LookupUser.tsx`). */
test.describe("find a user exactly", () => {
  test("by email goes to the account; an unknown phone says so", async ({ page }) => {
    await signInAsOperator(page);
    await page.goto("/admin/users");
    await page.getByText("Find by email, phone or sign-in provider").click();
    await expect(page.getByText(/one account that has exactly this/)).toBeVisible();
    await expectNoAxeViolations(page, "users list with the exact lookup open");

    await page.getByLabel(/^Email address/).fill("alice@example.org");
    await page.getByRole("button", { name: "Find the account" }).click();
    await expect(page).toHaveURL(/\/users\/%40alice%3Aexample\.org$/);
    await expect(page.getByRole("heading", { name: "Alice" })).toBeVisible();

    await page.goto("/admin/users");
    await page.getByText("Find by email, phone or sign-in provider").click();
    await page.getByRole("combobox", { name: "What you have" }).click();
    await page.getByRole("option", { name: "Phone number" }).click();
    await page.getByLabel(/^Phone number/).fill("15551234567");
    await page.getByRole("button", { name: "Find the account" }).click();
    await expect(page.getByRole("status")).toHaveText(
      "No account has 15551234567 as a verified phone number.",
    );
  });
});
