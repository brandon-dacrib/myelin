import { test, expect } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

// RFC 0017: an administrator offers a bridge, and each person gets their own. The mock seeds a
// WhatsApp offering, so this first stops offering it (which, with people still on it, asks for
// the bridge's name), then offers it again through the wizard, adds a bridge for someone and
// watches it come up. The mock's state lives in the page, so everything after sign-in is
// client-side navigation.

test.describe("Offer a bridge", () => {
  test("stop offering WhatsApp, offer it again, add one for a user, watch it become ready", async ({
    page,
  }) => {
    test.setTimeout(60_000);
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);

    await page.getByRole("link", { name: "Bridges" }).first().click();
    await expect(page.getByRole("heading", { name: "Bridges" })).toBeVisible();
    await expect(page.getByRole("table")).toContainText("@whatsappbot:example.org");
    await expectNoAxeViolations(page, "offered bridges");

    // The seeded offering, with people on it.
    await page.getByRole("table").getByRole("link", { name: "WhatsApp" }).click();
    await expect(page.getByRole("heading", { name: "WhatsApp" })).toBeVisible();
    await expect(
      page.getByText(
        "Anyone here can message @whatsappbot:example.org to get their own WhatsApp bridge.",
      ),
    ).toBeVisible();
    await expect(page.getByRole("table")).toContainText("ImagePullBackOff");
    await expectNoAxeViolations(page, "offering");

    await page
      .getByRole("table")
      .getByRole("button", { name: "Files for @alice:example.org" })
      .click();
    await expect(
      page.getByRole("region", { name: "Kubernetes manifest (Secret and Bridge)" }),
    ).toBeVisible();
    await expectNoAxeViolations(page, "offering: files");
    await page.getByRole("button", { name: "Done" }).click();

    await page.getByRole("button", { name: "Stop offering" }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Stop offering" }).click();
    const strong = page.getByRole("dialog", { name: /Remove everyone's WhatsApp bridge/ });
    await expect(strong).toContainText("still running");
    await expectNoAxeViolations(page, "offering: remove everyone's");
    await strong.getByLabel("Type WhatsApp to confirm").fill("WhatsApp");
    await strong.getByRole("button", { name: "Remove all and stop offering" }).click();
    await expect(page).toHaveURL(/\/bridges$/);
    await expect(page.getByRole("table").getByRole("link", { name: "WhatsApp" })).toHaveCount(0);

    // Offer it again.
    await page.getByRole("button", { name: "Offer a bridge" }).click();
    await expect(page).toHaveURL(/\/bridges\/new/);
    await page.getByRole("heading", { name: "Messaging" }).waitFor();
    // Already offered: iMessage can't be chosen again from here.
    await expect(page.getByRole("radio", { name: /iMessage/ })).toBeDisabled();
    await page.getByRole("radio", { name: /WhatsApp/ }).click();
    await expect(page.getByRole("radio", { name: /WhatsApp/ })).toContainText(
      "Each person gets their own",
    );
    await expectNoAxeViolations(page, "offer: kind");
    await page.getByRole("button", { name: "Continue" }).click();

    await expect(page.getByRole("heading", { name: "Access" })).toBeVisible();
    await expect(page.getByRole("radio", { name: /Everyone on this server/ })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await expectNoAxeViolations(page, "offer: access");
    await page.getByRole("button", { name: "Continue" }).click();

    await expect(page.getByRole("heading", { name: "Runtime" })).toBeVisible();
    await expect(page.getByRole("radio", { name: /Runs in this cluster/ })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await page.getByLabel("Image tag").fill("v0.12.1");
    await expectNoAxeViolations(page, "offer: runtime");
    await page.getByRole("button", { name: "Continue" }).click();

    await expect(page.getByRole("heading", { name: "Options" })).toBeVisible();
    await expect(page.getByRole("switch", { name: "Encryption" })).toBeChecked();
    await expect(page.getByRole("switch", { name: "Double puppeting" })).toBeChecked();
    await expectNoAxeViolations(page, "offer: options");
    await page.getByRole("button", { name: "Continue" }).click();

    await expect(page.getByRole("heading", { name: "Review" })).toBeVisible();
    await expect(page.getByText("Runs in this cluster")).toBeVisible();
    await expectNoAxeViolations(page, "offer: review");
    await page.getByRole("button", { name: "Offer WhatsApp" }).click();

    // The offering's page, with nobody on it yet.
    await expect(page).toHaveURL(/\/bridges\/offerings\/mautrix-whatsapp$/);
    await expect(page.getByRole("heading", { name: "WhatsApp" })).toBeVisible();
    await expect(page.getByText("Nobody has one yet")).toBeVisible();
    await expect(page.getByText("dock.mau.dev/mautrix/whatsapp:v0.12.1")).toBeVisible();

    await page.getByRole("button", { name: "Add for a user" }).click();
    const add = page.getByRole("dialog");
    await expectNoAxeViolations(page, "offering: add for a user");
    await add.getByLabel(/Matrix ID/).fill("@alice:example.org");
    await add.getByRole("button", { name: "Add bridge" }).click();
    await expect(add).toBeHidden();

    // It appears on its way, and the page follows it to ready without a reload.
    const row = page.getByRole("table").getByRole("row", { name: /@alice:example\.org/ });
    await expect(row).toBeVisible();
    await expect(row).toContainText(/Requested|Registered|Deploying|Starting/);
    await expect(row).toContainText("Healthy", { timeout: 25_000 });
    await expectNoAxeViolations(page, "offering: one ready");
    domGuard.assertClean();
  });

  test("when this server can't deploy, the wizard says why and offers it elsewhere", async ({
    page,
  }) => {
    const domGuard = installDomNestingGuard(page);
    await signInAsOperator(page);
    await page.evaluate(() =>
      window.__hsAdminMock?.setBridgeDeploymentTarget(
        false,
        "MYELIN_BRIDGES_NAMESPACE is not set: this server is not running in Kubernetes with the chart's bridges enabled.",
      ),
    );

    await page.getByRole("link", { name: "Bridges" }).first().click();
    await expect(page.getByText("This server can't run bridges itself.")).toBeVisible();
    await page.getByRole("button", { name: "Offer a bridge" }).click();
    await page.getByRole("radio", { name: /Signal/ }).click();
    await page.getByRole("button", { name: "Continue" }).click(); // -> access
    await page.getByRole("button", { name: "Continue" }).click(); // -> runtime

    await expect(page.getByRole("radio", { name: /Runs in this cluster/ })).toBeDisabled();
    await expect(page.getByRole("radio", { name: /Runs elsewhere/ })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await expect(page.getByText(/MYELIN_BRIDGES_NAMESPACE is not set/)).toBeVisible();
    await expectNoAxeViolations(page, "offer: runtime unavailable");

    await page.getByRole("button", { name: "Continue" }).click(); // -> options
    await page.getByRole("button", { name: "Continue" }).click(); // -> review
    await expect(page.getByText("Runs elsewhere")).toBeVisible();
    await page.getByRole("button", { name: "Offer Signal" }).click();
    await expect(page).toHaveURL(/\/bridges\/offerings\/mautrix-signal$/);
    await expect(page.getByText(/an administrator runs Signal for them/)).toBeVisible();
    domGuard.assertClean();
  });
});
