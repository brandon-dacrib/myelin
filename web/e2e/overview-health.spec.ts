import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations } from "./utils";

/** The server's own health checks on the Overview (`src/pages/dashboard/ServerHealthCard.tsx`). */
test.describe("overview health", () => {
  test("lists every check with its state in words", async ({ page }) => {
    await signInAsOperator(page);
    const card = page.getByRole("region", { name: "Health" });
    await expect(card.getByText(/Every probe answered: audit log, event stream/)).toBeVisible();
    await expect(card.getByText("User directory", { exact: true })).toBeVisible();
    await expect(card.getByText("Answering.").first()).toBeVisible();
    await expectNoAxeViolations(page, "overview with server health");
  });
});
