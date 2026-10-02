import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

/**
 * What an operator does after setting up a bridge for someone (RFC 0017 sections 4.1 and 4.2).
 * The owner set one up from the offering page and had no idea what came next; now the page says:
 * under the table, "Next steps for <person>" for everyone who has not signed in yet, with their
 * own bot and the catalogue's steps to relay; and the Add dialog stays open as a second step that
 * follows the new bridge to ready and then shows those steps. The mock walks a new instance to
 * ready in about ten seconds.
 */
test.describe("bridge next steps", () => {
  test("the offering page says what to tell each person who has not signed in", async ({
    page,
  }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/bridges/offerings/mautrix-whatsapp");
    await expect(page.getByRole("heading", { name: "Next steps" })).toBeVisible();

    // Alice has signed in, so there is nothing to tell her.
    await expect(page.getByText("Not signed in", { exact: true })).toBeVisible();
    await expect(page.locator("summary").filter({ hasText: "@alice:example.org" })).toHaveCount(0);

    // The operator's own bridge is ready: the invite to accept and the command to send.
    const ops = page.locator("summary").filter({ hasText: "Next steps for @ops:example.org" });
    await expect(ops).toContainText("Ready: sign in");
    await expect(ops).toContainText("(this is you)");
    await ops.click();
    const opsSteps = page.locator("details").filter({ has: ops });
    await expect(opsSteps.getByText(/This is you: accept the invite from/)).toContainText(
      "@whatsappbot_ops:example.org",
    );
    await expect(opsSteps.getByRole("list")).toContainText("login qr");

    // A bridge on its way says so and that the steps come later.
    const carol = page.locator("summary").filter({ hasText: "Next steps for @carol:example.org" });
    await expect(carol).toContainText("Setting up");
    await carol.click();
    await expect(
      page
        .locator("details")
        .filter({ has: carol })
        .getByText(/the steps appear here too/),
    ).toContainText("@whatsappbot_carol:example.org");

    await expectNoAxeViolations(page, "bridge offering, next steps open");
    domGuard.assertClean();
  });

  test("adding a bridge for someone follows it to ready and then shows what to tell them", async ({
    page,
  }) => {
    test.setTimeout(60_000);
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.goto("/admin/bridges/offerings/mautrix-whatsapp");

    await page.getByRole("button", { name: "Add for a user" }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel(/Matrix ID/).fill("@erin:example.org");
    await dialog.getByRole("button", { name: "Add bridge" }).click();

    // Step two: what is happening, with the bot that will invite them.
    await expect(dialog).toContainText("Setting up @erin:example.org's WhatsApp bridge");
    await expect(dialog).toContainText("@whatsappbot_erin:example.org");
    await expectNoAxeViolations(page, "add for a user: setting up");

    // It becomes ready without a reload, and the steps to relay replace the waiting words.
    await expect(dialog.getByText(/Tell them: their WhatsApp bridge is ready/)).toBeVisible({
      timeout: 25_000,
    });
    await expect(dialog.getByRole("list")).toContainText(
      "Start a direct chat with @whatsappbot_erin:example.org and send login qr",
    );
    await expect(
      dialog.getByRole("button", { name: "Copy as a message to send them" }),
    ).toBeVisible();
    await expectNoAxeViolations(page, "add for a user: ready, next steps");
    await dialog.getByRole("button", { name: "Done" }).click();
    await expect(dialog).toBeHidden();

    // The row says they have not signed in, and the same steps wait under the table.
    const row = page.getByRole("table").getByRole("row", { name: /@erin:example\.org/ });
    await expect(row).toContainText("Not signed in");
    await expect(
      page.locator("summary").filter({ hasText: "Next steps for @erin:example.org" }),
    ).toContainText("Ready: tell them how to sign in");
    domGuard.assertClean();
  });
});
