import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations } from "./utils";

/** The server's own health checks on the Overview (`src/pages/dashboard/ServerHealthCard.tsx`). */
test.describe("overview health", () => {
  test("says the server is fine in one sentence, with every check behind a disclosure", async ({
    page,
  }) => {
    await signInAsOperator(page);
    const card = page.getByRole("region", { name: "Health" });
    await expect(card.getByText(/Every probe answered: audit log, event stream/)).toBeVisible();
    // An ok server keeps the per-check rows closed: the sentence already said it all.
    await expect(card.getByText("User directory", { exact: true })).toBeHidden();
    await card.getByText("Show the checks").click();
    await expect(card.getByText("User directory", { exact: true })).toBeVisible();
    await expect(card.getByText("Answering.").first()).toBeVisible();
    await expectNoAxeViolations(page, "overview with server health");
  });

  test("says what the server is, in one line under the title", async ({ page }) => {
    await signInAsOperator(page);
    const line = page.getByTestId("server-line");
    await expect(line).toContainText("example.org");
    await expect(line).toContainText("a cluster of 3 replicas");
    await expect(line).toContainText(/up \d/);
  });
});
