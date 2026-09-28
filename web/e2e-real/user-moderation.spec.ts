import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { settle } from "./settle";

/**
 * A user's moderation-and-activity controls against a real `hs serve`, nothing mocked.
 *
 * A fresh user is made through the admin API; they create a room and send a message through
 * the client API. Then, through the interface: suspend them (their next send is refused with
 * `403 M_USER_SUSPENDED`), unsuspend them (it goes through), shadow-ban and lift it, set and
 * clear a message rate limit, read their sessions, rooms and statistics, redact what they sent
 * and follow the task until it has redacted at least one event, and mint a support token that is
 * shown once and does act as them (`/account/whoami`).
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts). Names carry
 * a per-run suffix so that a rerun does not collide. Screenshots go to
 * `docs/design/screenshots/user-moderation-*-real.png` as the record of the run.
 */
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const SHOTS = "../docs/design/screenshots";
const run = Date.now().toString(36);

/** The whole page, or just the viewport for a dialog (a full-page capture scrolls it away). */
async function shot(page: Page, name: string, fullPage = true) {
  await settle(page);
  await page.screenshot({ path: `${SHOTS}/user-moderation-${name}-real.png`, fullPage });
}

async function signIn(page: Page) {
  await page.goto("/");
  await page.getByRole("tab", { name: "Access token" }).click();
  await page.getByLabel("Access token").fill(adminToken!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();
}

/** Makes `localpart` through the admin API and signs in as them; their access token and id. */
async function makeUser(request: APIRequestContext, localpart: string) {
  const password = `hunter2-${localpart}-long-enough`;
  const created = await request.post("/api/v1/users", {
    headers: { authorization: `Bearer ${adminToken}`, "idempotency-key": `e2e-${localpart}` },
    data: { localpart, password },
  });
  expect(created.ok(), await created.text()).toBe(true);
  const login = await request.post("/_matrix/client/v3/login", {
    data: {
      type: "m.login.password",
      identifier: { type: "m.id.user", user: localpart },
      password,
    },
  });
  expect(login.ok(), await login.text()).toBe(true);
  return (await login.json()) as { access_token: string; user_id: string; device_id: string };
}

let txn = 0;

/** Sends a text message as `token`; the raw response, so a refusal can be read. */
function send(request: APIRequestContext, token: string, roomId: string, body: string) {
  txn += 1;
  return request.put(
    `/_matrix/client/v3/rooms/${encodeURIComponent(roomId)}/send/m.room.message/e2e-${run}-${txn}`,
    {
      headers: { authorization: `Bearer ${token}` },
      data: { msgtype: "m.text", body },
    },
  );
}

test.describe("User moderation against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed",
  );

  test("suspend, shadow-ban, rate limit, activity, redact and sign in as a user", async ({
    page,
    request,
  }) => {
    test.setTimeout(180_000);
    const subject = await makeUser(request, `moderated-${run}`);
    const roomName = `Moderation ${run}`;
    const created = await request.post("/_matrix/client/v3/createRoom", {
      headers: { authorization: `Bearer ${subject.access_token}` },
      data: { name: roomName, preset: "private_chat" },
    });
    expect(created.ok(), await created.text()).toBe(true);
    const { room_id: roomId } = (await created.json()) as { room_id: string };
    const first = await send(request, subject.access_token, roomId, "hello before moderation");
    expect(first.ok(), await first.text()).toBe(true);

    await signIn(page);
    await page.goto(`/admin/users/${encodeURIComponent(subject.user_id)}`);
    await expect(page.getByRole("heading", { name: subject.user_id, level: 1 })).toBeVisible();
    const moderation = page.getByRole("region", { name: "Moderation" });
    await expect(moderation.getByText("Not suspended")).toBeVisible();
    await shot(page, "page");

    // Suspend: their next send is refused.
    await moderation.getByRole("button", { name: "Suspend", exact: true }).click();
    const suspendDialog = page.getByRole("dialog", { name: `Suspend ${subject.user_id}?` });
    await suspendDialog.getByLabel("Reason").fill("e2e: suspension check");
    await shot(page, "suspend-dialog", false);
    await suspendDialog.getByRole("button", { name: "Suspend", exact: true }).click();
    await expect(suspendDialog).toBeHidden();
    await expect(moderation.getByText("Suspended", { exact: true })).toBeVisible();
    const refused = await send(request, subject.access_token, roomId, "sent while suspended");
    expect(refused.status()).toBe(403);
    expect(((await refused.json()) as { errcode?: string }).errcode).toBe("M_USER_SUSPENDED");
    await shot(page, "suspended");

    // Unsuspend: sending works again.
    await moderation.getByRole("button", { name: "Unsuspend" }).click();
    await expect(moderation.getByText("Not suspended")).toBeVisible();
    const again = await send(request, subject.access_token, roomId, "back after suspension");
    expect(again.ok(), await again.text()).toBe(true);

    // Shadow-ban, then lift it.
    await moderation.getByRole("button", { name: "Shadow-ban", exact: true }).click();
    const banDialog = page.getByRole("dialog", { name: `Shadow-ban ${subject.user_id}?` });
    await banDialog.getByLabel("Reason").fill("e2e: shadow-ban check");
    await banDialog.getByRole("button", { name: "Shadow-ban", exact: true }).click();
    await expect(banDialog).toBeHidden();
    await expect(moderation.getByText("Shadow-banned")).toBeVisible();
    await shot(page, "shadow-banned");
    await moderation.getByRole("button", { name: "Lift shadow-ban" }).click();
    await expect(moderation.getByRole("button", { name: "Shadow-ban", exact: true })).toBeVisible();

    // A rate limit: set, read back, clear.
    await expect(moderation.getByText("The server's own limits apply.")).toBeVisible();
    await moderation.getByLabel("Messages per second").fill("2");
    await moderation.getByLabel("Burst").fill("5");
    await moderation.getByRole("button", { name: "Save limit" }).click();
    await expect(moderation.getByText("2 messages a second, bursts of 5")).toBeVisible();
    const limit = await request.get(
      `/api/v1/users/${encodeURIComponent(subject.user_id)}/rate-limit`,
      { headers: { authorization: `Bearer ${adminToken}` } },
    );
    expect(await limit.json()).toMatchObject({ messages_per_second: 2, burst_count: 5 });
    await shot(page, "rate-limit");
    await moderation.getByRole("button", { name: "Clear override" }).click();
    await expect(moderation.getByText("The server's own limits apply.")).toBeVisible();

    // What they have been doing: a session, the room they made, what they sent.
    const activity = page.getByRole("region", { name: "Activity" });
    await expect(activity.getByRole("table", { name: "Sessions" })).toContainText(
      subject.device_id,
    );
    await activity.getByRole("tab", { name: "Rooms" }).click();
    const roomLink = activity
      .getByRole("table", { name: "Rooms" })
      .getByRole("link", { name: new RegExp(roomName) });
    await expect(roomLink).toBeVisible();
    await expect(roomLink).toHaveAttribute("href", new RegExp(encodeURIComponent(roomId)));
    await shot(page, "rooms");
    await activity.getByRole("tab", { name: "Statistics" }).click();
    await expect(activity.getByText("Events sent")).toBeVisible();
    await shot(page, "statistics");

    // Redact everything they sent, following the task until it ends.
    await moderation.getByRole("button", { name: "Redact messages…" }).click();
    const redact = page.getByRole("dialog", {
      name: `Redact messages sent by ${subject.user_id}?`,
    });
    await redact.getByLabel("Reason").fill("e2e: redaction check");
    await shot(page, "redact-dialog", false);
    await redact.getByRole("button", { name: "Redact messages" }).click();
    await expect(redact).toBeHidden();
    const followed = moderation.getByRole("region", { name: "Redacting messages" });
    await expect(followed.getByText("Succeeded")).toBeVisible({ timeout: 60_000 });
    const summary = (await followed
      .getByText(/^Redacted [\d,]+ of [\d,]+ events?\.$/)
      .textContent())!;
    const redacted = Number(/^Redacted ([\d,]+)/.exec(summary)![1]!.replace(/,/g, ""));
    expect(redacted).toBeGreaterThanOrEqual(1);
    await shot(page, "redacted");

    // Sign in as them: the token is shown once and acts as them.
    await moderation.getByRole("button", { name: "Sign in as user…" }).click();
    const ask = page.getByRole("dialog", { name: `Sign in as ${subject.user_id}?` });
    await ask.getByLabel(/^Reason/).fill("e2e: support token check");
    await shot(page, "login-as-dialog", false);
    await ask.getByRole("button", { name: "Create support token" }).click();
    const done = page.getByRole("dialog", { name: `Support token for ${subject.user_id}` });
    await expect(done).toContainText("cannot be shown again");
    const token = (await done.getByTestId("login-as-token").textContent())!.trim();
    expect(token.length).toBeGreaterThan(10);
    await shot(page, "login-as-token", false);
    const whoami = await request.get("/_matrix/client/v3/account/whoami", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(whoami.ok(), await whoami.text()).toBe(true);
    expect(((await whoami.json()) as { user_id: string }).user_id).toBe(subject.user_id);
    await done.getByRole("button", { name: "Done" }).click();
    await expect(done).toBeHidden();
    await expect(page.getByText(token)).toHaveCount(0);

    // The support session is listed and marked.
    await activity.getByRole("tab", { name: "Sessions" }).click();
    await expect(
      activity
        .getByRole("table", { name: "Sessions" })
        .getByText("Support session", { exact: true }),
    ).toBeVisible();
    await shot(page, "support-session");
  });
});
