import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { SHOTS } from "./screenshots";
import { settle } from "./settle";

/**
 * Erasing a user against a real `hs serve`, nothing mocked. Checked by what it does to the
 * person, through the client-server API, not only by what the page shows:
 *
 * - A fresh user with a display name, a device with uploaded keys and a room they created.
 * - From the page: Deactivate with "Also erase their data" ticked. Then: the Erased badge, the
 *   id as the heading (the display name is gone), an empty device list; their access token is
 *   refused; `GET /users/{id}` says `erased`, no display name, no devices; `/keys/query` no
 *   longer serves their keys; they are not in the room any more; Reactivate and reset-password
 *   answer 409 problem+json and the page offers neither.
 * - A second user, deactivated first through the API, erased from the "Erase this user's data"
 *   box the page shows for a deactivated account.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts). Names carry
 * a per-run suffix so a rerun does not collide. Screenshots go to
 * `user-erase-*-real.png` in `SHOTS` (`./screenshots.ts`).
 */
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const run = Date.now().toString(36);

async function shot(page: Page, name: string, fullPage = true) {
  await settle(page);
  await page.screenshot({ path: `${SHOTS}/user-erase-${name}-real.png`, fullPage });
}

async function signIn(page: Page) {
  await page.goto("/");
  await page.getByRole("tab", { name: "Access token" }).click();
  await page.getByLabel("Access token").fill(adminToken!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();
}

const admin = () => ({ authorization: `Bearer ${adminToken}` });
const bearer = (token: string) => ({ authorization: `Bearer ${token}` });

interface User {
  user_id: string;
  display_name?: string | null;
  deactivated?: boolean;
  erased?: boolean;
  device_count?: number;
}

/** Makes `localpart` through the admin API and signs in as them on `deviceId`. */
async function makeUser(request: APIRequestContext, localpart: string, deviceId: string) {
  const password = `hunter2-${localpart}-long-enough`;
  const created = await request.post("/api/v1/users", {
    headers: { ...admin(), "idempotency-key": `e2e-${localpart}` },
    data: { localpart, password, display_name: `Person ${localpart}` },
  });
  expect(created.ok(), await created.text()).toBe(true);
  const login = await request.post("/_matrix/client/v3/login", {
    data: {
      type: "m.login.password",
      identifier: { type: "m.id.user", user: localpart },
      password,
      device_id: deviceId,
      initial_device_display_name: `${deviceId} browser`,
    },
  });
  expect(login.ok(), await login.text()).toBe(true);
  return (await login.json()) as { access_token: string; user_id: string; device_id: string };
}

async function uploadKeys(
  request: APIRequestContext,
  token: string,
  userId: string,
  deviceId: string,
) {
  const uploaded = await request.post("/_matrix/client/v3/keys/upload", {
    headers: bearer(token),
    data: {
      device_keys: {
        user_id: userId,
        device_id: deviceId,
        algorithms: ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
        keys: {
          [`curve25519:${deviceId}`]: `curve-${deviceId}`,
          [`ed25519:${deviceId}`]: `ed-${deviceId}`,
        },
        signatures: { [userId]: { [`ed25519:${deviceId}`]: "sig" } },
      },
    },
  });
  expect(uploaded.ok(), await uploaded.text()).toBe(true);
}

async function adminUser(request: APIRequestContext, userId: string): Promise<User> {
  const got = await request.get(`/api/v1/users/${encodeURIComponent(userId)}`, {
    headers: admin(),
  });
  expect(got.ok(), await got.text()).toBe(true);
  return (await got.json()) as User;
}

async function adminDevices(request: APIRequestContext, userId: string) {
  const got = await request.get(`/api/v1/users/${encodeURIComponent(userId)}/devices`, {
    headers: admin(),
  });
  expect(got.ok(), await got.text()).toBe(true);
  return ((await got.json()) as { items: { device_id: string }[] }).items;
}

/** Everything the page and the API should say about an account once it is erased. */
async function expectErased(page: Page, request: APIRequestContext, userId: string) {
  await expect(page.getByText("Erased", { exact: true })).toBeVisible();
  await expect(page.getByText("Deactivated", { exact: true })).toBeVisible();
  await expect(page.getByRole("heading", { name: userId, level: 1 })).toBeVisible();
  await expect(page.getByText("No devices.")).toBeVisible();
  await expect(page.getByText("This account was erased")).toBeVisible();
  await expect(page.getByRole("button", { name: "Reactivate" })).toBeHidden();
  await expect(page.getByRole("button", { name: "Erase data" })).toBeHidden();
  await expect(page.getByRole("button", { name: "Deactivate" })).toBeHidden();
  await expect(page.getByRole("button", { name: "Reset password" })).toBeDisabled();

  const user = await adminUser(request, userId);
  expect(user.erased).toBe(true);
  expect(user.deactivated).toBe(true);
  expect(user.display_name ?? null).toBeNull();
  expect(user.device_count ?? 0).toBe(0);
  expect(await adminDevices(request, userId)).toEqual([]);

  // There is no way back: both answers are a 409 problem that says why.
  for (const [path, data] of [
    ["reactivate", undefined],
    ["reset-password", { password: "another-long-enough-one" }],
  ] as const) {
    const refused = await request.post(`/api/v1/users/${encodeURIComponent(userId)}/${path}`, {
      headers: { ...admin(), "idempotency-key": `e2e-${path}-${userId}-${run}` },
      data,
    });
    expect(refused.status(), `${path}: ${await refused.text()}`).toBe(409);
    expect(refused.headers()["content-type"]).toContain("application/problem+json");
    const problem = (await refused.json()) as { status: number; detail?: string };
    expect(problem.status).toBe(409);
    expect(problem.detail, path).toBeTruthy();
  }

  // Erasing again is a 200 no-op.
  const again = await request.post(`/api/v1/users/${encodeURIComponent(userId)}/deactivate`, {
    headers: { ...admin(), "idempotency-key": `e2e-again-${userId}-${run}` },
    data: { erase: true },
  });
  expect(again.status(), await again.text()).toBe(200);
  expect(((await again.json()) as User).erased).toBe(true);
}

test.describe("Erasing a user against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed",
  );

  test("deactivate and erase from the page removes everything but their messages", async ({
    page,
    request,
  }) => {
    test.setTimeout(180_000);
    const localpart = `erase-${run}`;
    const { access_token: token, user_id: userId } = await makeUser(request, localpart, "PHONE");
    await uploadKeys(request, token, userId, "PHONE");

    // A room they created and spoke in: they leave it, the message stays.
    const createdRoom = await request.post("/_matrix/client/v3/createRoom", {
      headers: bearer(token),
      data: { name: `Erase ${run}`, preset: "public_chat" },
    });
    expect(createdRoom.ok(), await createdRoom.text()).toBe(true);
    const roomId = ((await createdRoom.json()) as { room_id: string }).room_id;
    const sent = await request.put(
      `/_matrix/client/v3/rooms/${encodeURIComponent(roomId)}/send/m.room.message/txn-${run}`,
      { headers: bearer(token), data: { msgtype: "m.text", body: "still here afterwards" } },
    );
    expect(sent.ok(), await sent.text()).toBe(true);
    const eventId = ((await sent.json()) as { event_id: string }).event_id;

    await signIn(page);
    await page.goto(`/admin/users/${encodeURIComponent(userId)}`);
    await expect(
      page.getByRole("heading", { name: `Person ${localpart}`, level: 1 }),
    ).toBeVisible();
    await expect(
      page.getByRole("tabpanel", { name: "Sessions" }).getByText("PHONE browser", { exact: true }),
    ).toBeVisible();

    await page.getByRole("button", { name: "Deactivate" }).click();
    const dialog = page.getByRole("dialog", { name: `Deactivate ${userId}?` });
    await dialog.getByRole("checkbox", { name: /Also erase their data/ }).check();
    await expect(dialog.getByText(/every device, with its encryption keys/)).toBeVisible();
    await shot(page, "dialog", false);
    await dialog.getByRole("button", { name: "Deactivate and erase" }).click();
    await expect(dialog).toBeHidden();

    await expectErased(page, request, userId);
    await shot(page, "page");

    // Their session is gone, their keys are gone, they are out of the room, the message is not.
    const whoami = await request.get("/_matrix/client/v3/account/whoami", {
      headers: bearer(token),
    });
    expect(whoami.status()).toBe(401);
    const keys = await request.post("/_matrix/client/v3/keys/query", {
      headers: admin(),
      data: { device_keys: { [userId]: [] } },
    });
    if (keys.ok()) {
      const body = (await keys.json()) as { device_keys: Record<string, Record<string, unknown>> };
      expect(Object.keys(body.device_keys[userId] ?? {})).toEqual([]);
    }
    const memberships = await request.get(
      `/api/v1/users/${encodeURIComponent(userId)}/memberships?membership=join`,
      { headers: admin() },
    );
    expect(memberships.ok(), await memberships.text()).toBe(true);
    const joined = ((await memberships.json()) as { items: { room_id: string }[] }).items;
    expect(joined.map((m) => m.room_id)).not.toContain(roomId);
    const event = await request.get(
      `/api/v1/rooms/${encodeURIComponent(roomId)}/events/${encodeURIComponent(eventId)}`,
      { headers: admin() },
    );
    if (event.ok()) {
      const body = (await event.json()) as { content?: { body?: string } };
      expect(body.content?.body).toBe("still here afterwards");
    }

    // The list shows it, next to Deactivated.
    await page.goto(`/admin/users?q=${encodeURIComponent(localpart)}`);
    const row = page.getByRole("row").filter({ hasText: userId });
    await expect(row.getByText("Deactivated", { exact: true })).toBeVisible();
    await expect(row.getByText("Erased", { exact: true })).toBeVisible();
    await shot(page, "list");
  });

  test("an account deactivated earlier is erased from its own box", async ({ page, request }) => {
    test.setTimeout(180_000);
    const localpart = `erase-later-${run}`;
    const { user_id: userId } = await makeUser(request, localpart, "LAPTOP");
    const deactivated = await request.post(
      `/api/v1/users/${encodeURIComponent(userId)}/deactivate`,
      { headers: { ...admin(), "idempotency-key": `e2e-deactivate-${localpart}` }, data: {} },
    );
    expect(deactivated.ok(), await deactivated.text()).toBe(true);

    await signIn(page);
    await page.goto(`/admin/users/${encodeURIComponent(userId)}`);
    await expect(page.getByRole("button", { name: "Reactivate" })).toBeVisible();
    await page.getByRole("button", { name: "Erase data" }).click();
    const dialog = page.getByRole("dialog", { name: `Erase ${userId}'s data?` });
    await expect(dialog.getByText(/single-sign-on links/)).toBeVisible();
    await dialog.getByRole("button", { name: "Erase data" }).click();
    await expect(dialog).toBeHidden();

    await expectErased(page, request, userId);

    // On the record.
    const audit = await request.get("/api/v1/audit-log?action=users.deactivate", {
      headers: admin(),
    });
    if (audit.ok()) {
      const items = ((await audit.json()) as { items: { target: { id: string } }[] }).items;
      expect(items.map((i) => i.target.id)).toContain(userId);
    }
  });
});
