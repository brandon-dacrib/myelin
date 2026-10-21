import { test, expect } from "@playwright/test";
import { expectNoAxeViolations, installDomNestingGuard } from "./utils";

/**
 * The self-service integrations page (`/integrations/`, decision 0057), as a person who is not
 * an administrator uses it: sign in with their own account, see the webhooks in their rooms,
 * add one in a room of theirs (only the rooms they may set up are offered), read the URL once
 * with the sender's setup, and remove one after confirming. Against the mock user API; in the
 * preview build the page is `integrations.html` under the app's base.
 */
test.describe("Integrations page", () => {
  test("a user adds a webhook to a room of theirs and removes one", async ({ page }) => {
    const domGuard = installDomNestingGuard(page);
    await page.goto("/admin/integrations.html");
    await expect(page.getByRole("heading", { name: "Integrations" })).toBeVisible();
    await expectNoAxeViolations(page, "integrations sign-in");
    await page.getByLabel(/Username/).fill("alice");
    await page.getByLabel(/Password/).fill("a-password");
    await page.getByRole("button", { name: "Sign in" }).click();

    await expect(page.getByRole("heading", { name: "My webhooks" })).toBeVisible();
    await expect(page.getByTestId("signed-in-as")).toHaveText("@alice:example.org");
    const list = page.getByTestId("webhook-list");
    await expect(list).toContainText("uptime");
    await expect(list).toContainText("Ops");
    await expectNoAxeViolations(page, "my webhooks");

    await page.getByRole("button", { name: "Add a webhook" }).click();
    const dialog = page.getByRole("dialog", { name: "Add a webhook" });
    await dialog.getByRole("radio", { name: /Datadog monitors/ }).click();
    await expect(dialog).toContainText("2 of your 4 rooms can take one");
    await dialog.getByRole("combobox", { name: /Room/ }).click();
    const listbox = page.getByRole("listbox");
    await expect(listbox.getByRole("option", { name: "Team chat" })).toHaveCount(0);
    await listbox.getByRole("option", { name: "Deploys" }).click();
    await dialog.getByLabel(/^Name/).fill("datadog-alerts");
    await expectNoAxeViolations(page, "add a webhook as a user");
    await dialog.getByRole("button", { name: "Connect" }).click();

    const made = page.getByRole("dialog", { name: "Webhook datadog-alerts is connected" });
    await expect(made).toContainText("it will not be shown again");
    await expect(made.getByTestId("webhook-url")).toContainText(
      /https:\/\/hooks\.example\.org\/webhook\/[0-9a-f-]{36}/,
    );
    await expect(made.getByTestId("webhook-setup")).toContainText("New Webhook");
    await expectNoAxeViolations(page, "webhook made as a user");
    await made.getByRole("button", { name: "Done" }).click();
    await expect(list).toContainText("datadog-alerts");
    await expect(list).toContainText("Deploys");
    await expect(page.locator("body")).not.toContainText("/webhook/");

    await page.getByRole("button", { name: "Remove uptime" }).click();
    const confirm = page.getByRole("dialog", { name: "Remove uptime?" });
    await expect(confirm).toContainText("nothing more from it lands in Ops");
    await expectNoAxeViolations(page, "remove a webhook as a user");
    await confirm.getByRole("button", { name: "Remove" }).click();
    await expect(list).not.toContainText("uptime");
    await expect(list).toContainText("datadog-alerts");

    // Phone width: the page still fits and passes the checks.
    await page.setViewportSize({ width: 390, height: 844 });
    await expect(page.getByRole("heading", { name: "My webhooks" })).toBeVisible();
    await expectNoAxeViolations(page, "my webhooks at phone width");
    domGuard.assertClean();
  });
});
