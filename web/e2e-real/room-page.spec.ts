import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { SHOTS } from "./screenshots";
import { settle } from "./settle";

/**
 * The room page against a real `hs serve`, nothing mocked: a user makes a room and talks in it
 * through the client-server API, and the administrator reads its state and timeline, adds an
 * alias, sees a single forward extremity, purges the older messages (a task the page follows)
 * and then deletes the room (another task), after which the room is gone: the admin API answers
 * 404 for it and the user cannot join it again.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts). The user is
 * made through the admin API, so registration may be closed. Names carry a per-run suffix.
 * Screenshots go to `rooms-*-real.png` in `SHOTS` (`./screenshots.ts`) as the record of the run.
 */
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const run = Date.now().toString(36);

async function shot(page: Page, name: string, fullPage = true) {
  await settle(page);
  await page.screenshot({ path: `${SHOTS}/rooms-${name}-real.png`, fullPage });
}

async function signIn(page: Page) {
  await page.goto("/");
  await page.getByRole("tab", { name: "Access token" }).click();
  await page.getByLabel("Access token").fill(adminToken!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();
}

/** Makes `localpart` through the admin API and signs in as them. */
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
  return (await login.json()) as { access_token: string; user_id: string };
}

async function send(request: APIRequestContext, token: string, roomId: string, body: string) {
  const response = await request.put(
    `/_matrix/client/v3/rooms/${encodeURIComponent(roomId)}/send/m.room.message/${run}-${body}`,
    { headers: { authorization: `Bearer ${token}` }, data: { msgtype: "m.text", body } },
  );
  expect(response.ok(), await response.text()).toBe(true);
}

/** `date` as a `datetime-local` value, in the browser's (and this process's) local time. */
function localInput(date: Date): string {
  const pad = (n: number) => String(n).padStart(2, "0");
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}` +
    `T${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
  );
}

test.describe("The room page against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed",
  );

  test("read, alias, extremities, purge history, delete", async ({ page, request }) => {
    test.setTimeout(180_000);
    const alice = await makeUser(request, `roomy-${run}`);
    const server = alice.user_id.slice(alice.user_id.indexOf(":") + 1);
    const name = `Room page ${run}`;
    const created = await request.post("/_matrix/client/v3/createRoom", {
      headers: { authorization: `Bearer ${alice.access_token}` },
      data: { name, preset: "public_chat" },
    });
    expect(created.ok(), await created.text()).toBe(true);
    const roomId = ((await created.json()) as { room_id: string }).room_id;

    for (const body of ["old-1", "old-2", "old-3"]) {
      await send(request, alice.access_token, roomId, body);
    }
    // Whole seconds apart, since the purge dialog's time has a second's resolution.
    await new Promise((resolve) => setTimeout(resolve, 2_000));
    const cutoff = new Date();
    await new Promise((resolve) => setTimeout(resolve, 2_000));
    for (const body of ["new-1", "new-2"]) {
      await send(request, alice.access_token, roomId, body);
    }

    await signIn(page);
    await page.goto(`/admin/rooms/${encodeURIComponent(roomId)}`);
    await expect(page.getByRole("heading", { name, level: 1 })).toBeVisible();
    await expect(page.getByText(alice.user_id).first()).toBeVisible();
    await shot(page, "overview");

    await page.getByRole("tab", { name: "State" }).click();
    await expect(
      page.getByRole("table", { name: "Room state" }).getByText("m.room.create"),
    ).toBeVisible();
    await shot(page, "state");

    await page.getByRole("tab", { name: "Timeline" }).click();
    const messages = page.getByRole("list", { name: "Messages" });
    await expect(messages.getByText("new-2")).toBeVisible();
    await expect(messages.getByText("old-1")).toBeVisible();
    await messages.getByText("old-2").click();
    const context = page.getByRole("complementary", { name: "Event in context" });
    await expect(context.getByText("old-1")).toBeVisible();
    await expect(context.getByText("old-3")).toBeVisible();
    await shot(page, "timeline");

    await page.getByRole("tab", { name: "Aliases" }).click();
    const alias = `#rp-${run}:${server}`;
    await page.getByLabel("New alias").fill(alias);
    await page.getByRole("button", { name: "Add alias" }).click();
    await expect(page.getByRole("list", { name: "Aliases" }).getByText(alias)).toBeVisible();
    const resolved = await request.get(
      `/_matrix/client/v3/directory/room/${encodeURIComponent(alias)}`,
    );
    expect(((await resolved.json()) as { room_id: string }).room_id).toBe(roomId);
    await shot(page, "aliases");

    await page.getByRole("tab", { name: "Extremities" }).click();
    await expect(
      page.getByRole("table", { name: "Forward extremities" }).getByRole("row"),
    ).toHaveCount(2);
    await expect(page.getByText(/One extremity/)).toBeVisible();
    await shot(page, "extremities");

    await page.getByRole("button", { name: "Purge history" }).click();
    const purge = page.getByRole("dialog");
    await purge.getByLabel("Purge messages sent before").fill(localInput(cutoff));
    await purge.getByRole("checkbox", { name: /own users/ }).check();
    await shot(page, "purge-dialog", false);
    await purge.getByRole("button", { name: "Purge history" }).click();
    await expect(purge.getByText(/^Purged 3 events/)).toBeVisible({ timeout: 60_000 });
    await shot(page, "purge-done", false);
    await purge.getByRole("button", { name: "Close" }).first().click();

    await page.getByRole("tab", { name: "Timeline" }).click();
    await expect(messages.getByText("new-2")).toBeVisible();
    await expect(messages.getByText("old-1")).toHaveCount(0);
    // And the room's own member sees the same through the client-server API.
    const history = await request.get(
      `/_matrix/client/v3/rooms/${encodeURIComponent(roomId)}/messages?dir=b&limit=50`,
      { headers: { authorization: `Bearer ${alice.access_token}` } },
    );
    const bodies = ((await history.json()) as { chunk: { content: { body?: string } }[] }).chunk
      .map((e) => e.content.body)
      .filter(Boolean);
    expect(bodies).toContain("new-2");
    expect(bodies).not.toContain("old-1");
    await shot(page, "timeline-after-purge");

    await page.getByRole("button", { name: "Delete room" }).click();
    const del = page.getByRole("dialog");
    await del.getByRole("checkbox", { name: /Block it/ }).check();
    await del.getByLabel(/to confirm/).fill(name);
    await shot(page, "delete-dialog", false);
    await del.getByRole("button", { name: "Delete room" }).click();
    await expect(page.getByRole("heading", { name: "Rooms", level: 1 })).toBeVisible({
      timeout: 60_000,
    });
    await shot(page, "after-delete");

    const gone = await request.get(`/api/v1/rooms/${encodeURIComponent(roomId)}`, {
      headers: { authorization: `Bearer ${adminToken}` },
    });
    expect(gone.status()).toBe(404);
    const rejoin = await request.post(`/_matrix/client/v3/join/${encodeURIComponent(roomId)}`, {
      headers: { authorization: `Bearer ${alice.access_token}` },
      data: {},
    });
    expect(rejoin.ok()).toBe(false);
  });
});
