import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

/**
 * A bridge's dead letters, from its Transactions tab (`src/pages/bridges/BridgeTransactions.tsx`):
 * what each queued entry carries and where, and replay and discard, each confirmed first with
 * how many and how old (OpenAPI 0.1.18, `appservices.discard`). The mock's Signal bridge is down
 * and has two dead letters, one three days old.
 */
test.describe("bridge dead letters", () => {
  test("the page says why the bridge is down, and what each dead letter carries", async ({
    page,
  }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/bridges/signal");
    await expect(page.getByTestId("health-reason")).toHaveText(
      /^Down: The last 24 transactions to it failed/,
    );
    await page.getByRole("tab", { name: "Transactions" }).click();
    await expect(page.getByTestId("dead-letters")).toContainText(
      "2 dead-lettered transactions, the oldest queued 3d ago.",
    );
    const oldest = page.getByTestId("backlog-entry-tx_700");
    await expect(oldest).toContainText("Carries 48 events");
    await expect(oldest).toContainText("!OGEhHVWSdvArJzumhm:matrix.org");
    await expect(oldest).toContainText("10 attempts");
    await expectNoAxeViolations(page, "bridge transactions with dead letters");

    // Replay confirms first and says the bridge gets three-day-old events; cancelling sends nothing.
    await page.getByRole("button", { name: "Replay all" }).click();
    const replay = page.getByRole("dialog", { name: "Replay 2 dead-lettered transactions?" });
    await expect(replay).toContainText("The oldest was queued 3d ago");
    await expectNoAxeViolations(page, "replay confirmation");
    await replay.getByRole("button", { name: "Cancel" }).click();
    await expect(replay).toBeHidden();
    await expect(page.getByTestId("backlog-entry-tx_700")).toBeVisible();
    domGuard.assertClean();
  });

  test("discard one dead letter, then the rest, and the list says the queue is clear", async ({
    page,
  }) => {
    await signInAsOperator(page);
    await page.goto("/admin/bridges/signal");
    await page.getByRole("tab", { name: "Transactions" }).click();

    await page.getByRole("button", { name: "Discard transaction tx_701" }).click();
    const one = page.getByRole("dialog", { name: "Discard transaction tx_701?" });
    await expect(one).toContainText("This cannot be undone.");
    await one.getByRole("button", { name: "Discard" }).click();
    await expect(page.getByText("Transaction tx_701 discarded").first()).toBeVisible();
    await expect(page.getByTestId("backlog-entry-tx_701")).toHaveCount(0);

    await page.getByRole("button", { name: "Discard all dead letters" }).click();
    const all = page.getByRole("dialog", { name: "Discard 1 dead-lettered transaction?" });
    await expect(all).toContainText("This cannot be undone.");
    await expectNoAxeViolations(page, "discard confirmation");
    await all.getByRole("button", { name: "Discard all" }).click();
    await expect(page.getByText("Discarded 1 dead-lettered transaction").first()).toBeVisible();
    await expect(page.getByText("No pending or dead-lettered transactions.")).toBeVisible();
    await expect(page.getByTestId("dead-letters")).toHaveCount(0);

    await page.getByRole("link", { name: "Registrations" }).first().click();
    await expect(page.getByTestId("queue-signal")).toHaveText(/Up to date/);
  });
});
