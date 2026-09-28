import { test, expect, type Page } from "@playwright/test";
import { signInAsOperator, expectNoAxeViolations, installDomNestingGuard } from "./utils";

// The moderation-and-activity half of a user's page, on the mock: suspend and unsuspend,
// shadow-ban and lift it, set and clear a message rate limit, mint a support token that is shown
// exactly once, redact everything they sent while following the task to its result, delete all
// their media, and read their sessions, rooms and statistics.

const ALICE = "@alice:example.org";

async function openAlice(page: Page) {
  await signInAsOperator(page);
  await page
    .getByRole("navigation", { name: "Primary" })
    .getByRole("link", { name: "Users" })
    .click();
  await page.getByRole("link", { name: ALICE }).click();
  await expect(page.getByRole("heading", { name: "Alice", level: 1 })).toBeVisible();
}

test.describe("User moderation", () => {
  test("suspend, shadow-ban and rate limit, reflected on the page and in the list", async ({
    page,
  }) => {
    const guard = installDomNestingGuard(page);
    await openAlice(page);
    const moderation = page.getByRole("region", { name: "Moderation" });
    await expect(moderation.getByText("Not suspended")).toBeVisible();
    await expectNoAxeViolations(page, "user page with moderation");

    // Suspend, with a reason, from a dialog that names them.
    await moderation.getByRole("button", { name: "Suspend", exact: true }).click();
    const suspendDialog = page.getByRole("dialog", { name: `Suspend ${ALICE}?` });
    await suspendDialog.getByLabel("Reason").fill("flooding #general");
    await expectNoAxeViolations(page, "suspend dialog");
    await suspendDialog.getByRole("button", { name: "Suspend", exact: true }).click();
    await expect(suspendDialog).toBeHidden();
    await expect(moderation.getByText("Suspended", { exact: true })).toBeVisible();
    await expect(moderation.getByRole("button", { name: "Unsuspend" })).toBeVisible();

    // The list shows it too, and "Suspended only" finds them.
    await page.getByRole("link", { name: "Users" }).first().click();
    const aliceRow = page.getByRole("row").filter({ hasText: ALICE });
    await expect(aliceRow.getByText("Suspended", { exact: true })).toBeVisible();
    await page.getByRole("switch", { name: "Suspended only" }).click();
    await expect(page).toHaveURL(/suspended=true/);
    await expect(page.getByRole("link", { name: ALICE })).toBeVisible();
    await expect(page.getByRole("link", { name: "@bot:example.org" })).toBeHidden();
    await page.getByRole("link", { name: ALICE }).click();

    await moderation.getByRole("button", { name: "Unsuspend" }).click();
    await expect(moderation.getByText("Not suspended")).toBeVisible();

    // Shadow-ban and lift it.
    await moderation.getByRole("button", { name: "Shadow-ban", exact: true }).click();
    const banDialog = page.getByRole("dialog", { name: `Shadow-ban ${ALICE}?` });
    await banDialog.getByRole("button", { name: "Shadow-ban", exact: true }).click();
    await expect(banDialog).toBeHidden();
    await expect(page.getByText("Shadow-banned").first()).toBeVisible();
    await moderation.getByRole("button", { name: "Lift shadow-ban" }).click();
    await expect(moderation.getByRole("button", { name: "Shadow-ban", exact: true })).toBeVisible();

    // A rate limit: set it, see it worded, clear it.
    await expect(moderation.getByText("The server's own limits apply.")).toBeVisible();
    await moderation.getByLabel("Messages per second").fill("2");
    await moderation.getByLabel("Burst").fill("5");
    await moderation.getByRole("button", { name: "Save limit" }).click();
    await expect(moderation.getByText("2 messages a second, bursts of 5")).toBeVisible();
    await moderation.getByRole("button", { name: "Clear override" }).click();
    await expect(moderation.getByText("The server's own limits apply.")).toBeVisible();
    await expectNoAxeViolations(page, "user page after moderation");
    guard.assertClean();
  });

  test("sign in as the user: the token is shown once and the session is marked", async ({
    page,
  }) => {
    const guard = installDomNestingGuard(page);
    await openAlice(page);
    const moderation = page.getByRole("region", { name: "Moderation" });
    await moderation.getByRole("button", { name: "Sign in as user…" }).click();

    const ask = page.getByRole("dialog", { name: `Sign in as ${ALICE}?` });
    await expect(ask.getByRole("note")).toContainText("audit log");
    await ask.getByLabel(/^Reason/).fill("ticket 4412: cannot see a room");
    await expectNoAxeViolations(page, "login-as dialog");
    await ask.getByRole("button", { name: "Create support token" }).click();

    const done = page.getByRole("dialog", { name: `Support token for ${ALICE}` });
    await expect(done).toContainText("cannot be shown again");
    const token = (await done.getByTestId("login-as-token").textContent())!.trim();
    expect(token).toMatch(/^mock_support_/);
    await expect(done.getByRole("button", { name: "Copy access token" })).toBeVisible();
    await expectNoAxeViolations(page, "support token dialog");
    await done.getByRole("button", { name: "Done" }).click();
    await expect(done).toBeHidden();
    await expect(page.getByText(token)).toHaveCount(0);

    // Opening it again asks afresh; the old token is gone.
    await moderation.getByRole("button", { name: "Sign in as user…" }).click();
    await expect(page.getByRole("dialog", { name: `Sign in as ${ALICE}?` })).toBeVisible();
    await expect(page.getByText(token)).toHaveCount(0);
    await page.getByRole("button", { name: "Cancel" }).click();

    // The session it made is listed and marked as a support session.
    const activity = page.getByRole("region", { name: "Activity" });
    await expect(
      activity.getByRole("table", { name: "Sessions" }).getByText("Support session"),
    ).toBeVisible();
    guard.assertClean();
  });

  test("redact everything they sent, following the task; delete their media; read their activity", async ({
    page,
  }) => {
    test.setTimeout(60_000);
    const guard = installDomNestingGuard(page);
    await openAlice(page);
    const moderation = page.getByRole("region", { name: "Moderation" });

    await moderation.getByRole("button", { name: "Redact messages…" }).click();
    const redact = page.getByRole("dialog", { name: `Redact messages sent by ${ALICE}?` });
    await expect(redact).toContainText("cannot be undone");
    await redact.getByLabel("Reason").fill("spam wave");
    await expectNoAxeViolations(page, "redact dialog");
    await redact.getByRole("button", { name: "Redact messages" }).click();
    await expect(redact).toBeHidden();

    const followed = moderation.getByRole("region", { name: "Redacting messages" });
    await expect(followed.getByRole("progressbar")).toBeVisible();
    await expect(followed.getByRole("progressbar")).toHaveAccessibleName(/of 1,204 events/);
    await expect(followed).toContainText("Redacted 1,204 of 1,204 events.", { timeout: 15_000 });
    await expect(followed.getByText("Succeeded")).toBeVisible();

    await moderation.getByRole("button", { name: "Delete all media…" }).click();
    const media = page.getByRole("dialog", { name: `Delete all media uploaded by ${ALICE}?` });
    await expect(media).toContainText("They have uploaded 1 file");
    await expectNoAxeViolations(page, "delete media dialog");
    await media.getByRole("button", { name: "Delete all media" }).click();
    await expect(moderation.getByRole("region", { name: "Deleting media" })).toContainText(
      "Deleted 1 file (2.3 MiB).",
    );

    // What they have been doing.
    const activity = page.getByRole("region", { name: "Activity" });
    await activity.getByRole("tab", { name: "Rooms" }).click();
    await expect(
      activity.getByRole("table", { name: "Rooms" }).getByRole("link", { name: /General/ }),
    ).toBeVisible();
    await activity.getByRole("tab", { name: "Statistics" }).click();
    await expect(activity.getByText("Events sent")).toBeVisible();
    await expect(activity.getByText("1,204")).toBeVisible();
    await activity.getByRole("tab", { name: "Media" }).click();
    await expect(activity.getByText("Nothing uploaded.").first()).toBeVisible();
    await expectNoAxeViolations(page, "user page after cleanup");

    // The redaction is a task on the Tasks page.
    await followed.getByRole("link", { name: "Open in Tasks" }).click();
    await expect(
      page.getByRole("heading", { name: "Redact a user's messages", level: 1 }),
    ).toBeVisible();
    guard.assertClean();
  });
});
