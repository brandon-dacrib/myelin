import { test, expect, type Page } from "@playwright/test";
import { settle } from "./settle";

/**
 * RFC 0017 through the interface against a real `hs serve`, nothing mocked: offer WhatsApp
 * (the server cannot deploy, so it runs elsewhere and the Runtime step quotes the server's own
 * reason), the offering's page, add a bridge for a registered user, watch the row reach
 * "Starting" as the manager registers it, open its files, remove it, stop offering.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts), a server
 * with no WhatsApp offering yet, and a local user `@alice:<server>` (register one through the
 * client API; `HS_REAL_USER` overrides the Matrix ID). Screenshots go to
 * `docs/design/screenshots/bridge-offerings-*-real.png`: the record of the run.
 */
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const user = process.env.HS_REAL_USER ?? "@alice:example.org";
const SHOTS = "../docs/design/screenshots";

/** The whole page, or just the viewport for a dialog (a full-page capture scrolls it away). */
async function shot(page: Page, name: string, fullPage = true) {
  await settle(page);
  await page.screenshot({ path: `${SHOTS}/bridge-offerings-${name}-real.png`, fullPage });
}

test.describe("bridge offerings against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed",
  );

  test("offer WhatsApp elsewhere, add one for a user, files, remove, stop offering", async ({
    page,
  }) => {
    test.setTimeout(180_000);
    await page.goto("/");
    await page.getByRole("tab", { name: "Access token" }).click();
    await page.getByLabel("Access token").fill(adminToken!);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();

    // Nothing offered yet, and the server says it cannot run bridges itself.
    await page.goto("/admin/bridges");
    await expect(page.getByRole("heading", { name: "Bridges" })).toBeVisible();
    await expect(page.getByText("This server can't run bridges itself.")).toBeVisible();
    await shot(page, "list-empty");

    // Offer a bridge: WhatsApp, for everyone, elsewhere (the only runtime there is).
    await page.getByRole("button", { name: "Offer a bridge" }).first().click();
    await expect(page).toHaveURL(/\/bridges\/new/);
    await page.getByRole("heading", { name: "Messaging" }).waitFor();
    await page.getByRole("radio", { name: /WhatsApp/ }).click();
    await expect(page.getByRole("radio", { name: /WhatsApp/ })).toContainText(
      "Each person gets their own",
    );
    await page.getByRole("button", { name: "Continue" }).click();
    await expect(page.getByRole("heading", { name: "Access" })).toBeVisible();
    await page.getByRole("button", { name: "Continue" }).click();
    await expect(page.getByRole("heading", { name: "Runtime" })).toBeVisible();
    await expect(page.getByRole("radio", { name: /Runs in this cluster/ })).toBeDisabled();
    await expect(page.getByRole("radio", { name: /Runs elsewhere/ })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    // The server's own reason, not the interface's guess.
    await expect(page.getByRole("note")).toContainText(
      "not running in Kubernetes with the chart's bridges enabled",
    );
    await shot(page, "wizard-runtime");
    await page.getByRole("button", { name: "Continue" }).click();
    await expect(page.getByRole("heading", { name: "Options" })).toBeVisible();
    await page.getByRole("button", { name: "Continue" }).click();
    await expect(page.getByRole("heading", { name: "Review" })).toBeVisible();
    await expect(page.getByText("Runs elsewhere")).toBeVisible();
    await page.getByRole("button", { name: "Offer WhatsApp" }).click();

    // The offering's page, from the server.
    await expect(page).toHaveURL(/\/bridges\/offerings\/mautrix-whatsapp$/);
    await expect(page.getByRole("heading", { name: "WhatsApp" })).toBeVisible();
    await expect(page.getByText("Nobody has one yet")).toBeVisible();
    await expect(
      page.getByText(/Anyone here can message @whatsappbot:[^ ]+ to get their own WhatsApp bridge/),
    ).toBeVisible();
    await expect(page.getByText("dock.mau.dev/mautrix/whatsapp:latest")).toBeVisible();
    await shot(page, "offering");

    // Someone from another server is refused by the server, in the field.
    await page.getByRole("button", { name: "Add for a user" }).click();
    const add = page.getByRole("dialog");
    await add.getByLabel(/Matrix ID/).fill("@bob:elsewhere.net");
    await add.getByRole("button", { name: "Add bridge" }).click();
    await expect(add).toContainText("not a user of this server");
    await shot(page, "add-refused", false);

    // A registered user gets one: the row appears and the manager takes it to Starting (there
    // is nothing here to run it; someone downloads the files and does).
    await add.getByLabel(/Matrix ID/).fill(user);
    await add.getByRole("button", { name: "Add bridge" }).click();
    await expect(add).toBeHidden();
    const row = page.getByRole("table").getByRole("row", { name: new RegExp(user) });
    await expect(row).toBeVisible();
    await expect(row).toContainText("Starting", { timeout: 30_000 });
    await expect(row).toContainText("waiting for someone to run it");
    await expect(row).toContainText("Elsewhere");
    await shot(page, "instance-starting");

    await page.getByRole("button", { name: `Files for ${user}` }).click();
    const files = page.getByRole("dialog");
    await expect(files.getByRole("region", { name: "registration.yaml" })).toContainText(
      "id: whatsapp-alice",
    );
    await expect(files.getByRole("region", { name: "registration.yaml" })).toContainText(
      `io.myelin.bridge_instance: '${user}'`,
    );
    const config = files.getByRole("region", { name: "config.yaml" });
    await expect(config).toContainText("id: whatsapp-alice");
    await expect(config).toContainText(`"${user}": admin`);
    // A server with no public base URL tells the bridge its bound address, never nothing.
    await expect(config).toContainText(/address: http:\/\/127\.0\.0\.1:\d+/);
    await expect(
      files.getByRole("region", { name: "Kubernetes manifest (Secret and Bridge)" }),
    ).toContainText("kind: Bridge");
    await shot(page, "files", false);
    await files.getByRole("button", { name: "Done" }).click();

    // Remove it, then stop offering: the table empties, and the offering leaves the list.
    await page.getByRole("button", { name: `Remove ${user}` }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Remove bridge" }).click();
    await expect(page.getByText("Nobody has one yet")).toBeVisible({ timeout: 15_000 });
    await page.getByRole("button", { name: "Stop offering" }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Stop offering" }).click();
    await expect(page).toHaveURL(/\/bridges$/);
    await expect(page.getByRole("table").getByRole("link", { name: "WhatsApp" })).toHaveCount(0);
    await shot(page, "list-after");
  });
});
