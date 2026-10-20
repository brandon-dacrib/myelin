import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

/**
 * One Hookshot, many webhooks: from the offering's page an administrator adds a webhook for a
 * source (here Datadog) in a new room, is shown the URL once with the sender's setup, sees it
 * listed with its source, and removes a connection after confirming. The mock is seeded with the
 * offering from inside the page (its state lives there).
 */
test.describe("Hookshot webhooks", () => {
  test("add a Datadog webhook in a new room, read the URL once, remove a connection", async ({
    page,
  }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.evaluate(() =>
      fetch("/api/v1/bridge-offerings/matrix-hookshot", {
        method: "PUT",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          runtime: "cluster",
          enabled: true,
          options: { public_url: "https://hooks.example.org" },
        }),
      }).then((r) => r.status),
    );
    // The mock's state lives in the page, so from here on navigation is client-side.
    await page.getByRole("link", { name: "Bridges" }).first().click();
    await page.getByRole("table").getByRole("link", { name: "Hookshot" }).click();
    await expect(page.getByRole("heading", { name: "The bridge" })).toBeVisible();
    const connections = page.getByTestId("connections");
    await expect(connections.getByRole("table")).toContainText("brandon-dacrib/myelin");
    await expectNoAxeViolations(page, "Hookshot offering with its connections");

    await connections.getByRole("button", { name: "Add a webhook" }).click();
    const dialog = page.getByRole("dialog", { name: "Add a webhook" });
    await dialog.getByRole("radio", { name: /Datadog monitors/ }).click();
    await dialog.getByRole("radio", { name: /A new room/ }).click();
    await dialog.getByLabel(/Room name/).fill("Alerts");
    await dialog.getByLabel(/Invite/).fill("@alice:example.org");
    await dialog.getByLabel(/^Name/).fill("datadog-alerts");
    await expectNoAxeViolations(page, "add a webhook");
    await dialog.getByRole("button", { name: "Connect" }).click();

    const made = page.getByRole("dialog", { name: "Webhook datadog-alerts is connected" });
    await expect(made).toContainText("it will not be shown again");
    await expect(made.getByTestId("webhook-url")).toContainText(
      /https:\/\/hooks\.example\.org\/webhook\/[0-9a-f-]{36}/,
    );
    await expect(made.getByTestId("webhook-setup")).toContainText("New Webhook");
    await expectNoAxeViolations(page, "webhook made");
    await made.getByRole("button", { name: "Done" }).click();

    const row = connections.getByRole("row").filter({ hasText: "datadog-alerts" });
    await expect(row).toContainText("Source: datadog");
    await expect(row).toContainText("Alerts");
    await expect(page.locator("body")).not.toContainText("/webhook/");

    // Remove the repository connection: confirmed first, then the row says Removed.
    await connections.getByRole("button", { name: "Remove brandon-dacrib/myelin" }).click();
    const confirm = page.getByRole("dialog", { name: "Remove brandon-dacrib/myelin?" });
    await expect(confirm).toContainText("Nothing more from it lands in myelin pull requests");
    await expectNoAxeViolations(page, "remove a connection");
    await confirm.getByRole("button", { name: "Remove" }).click();
    const repo = connections.getByRole("row").filter({ hasText: "brandon-dacrib/myelin" });
    await expect(repo).toContainText("Removed");
    await expect(repo.getByRole("button", { name: /^Remove / })).toHaveCount(0);
    domGuard.assertClean();
  });
});
