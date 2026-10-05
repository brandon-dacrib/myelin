import { test, expect, type APIRequestContext } from "@playwright/test";
import { SHOTS } from "./screenshots";
import { settle } from "./settle";

/**
 * "How they appear" on a user's page against a real `hs serve`, nothing mocked. Checked by what
 * the person and the people in their rooms see, through the client-server API:
 *
 * - The name and avatar set in place are the person's `/profile`, and their `m.room.member`
 *   event in a room they share with somebody else carries them (the server re-sends it, as it
 *   does when the person renames themself).
 * - The kind of account is recorded and read back.
 * - Creating an account says, as the name is typed, that it is taken, free, or why it can
 *   never be one.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts), and a
 * server with open registration or the admin token's `users.create`. Names carry a per-run
 * suffix. Screenshots go to `users-profile-*-real.png` in `SHOTS` (`./screenshots.ts`).
 */
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const run = Date.now().toString(36);
const admin = () => ({ authorization: `Bearer ${adminToken}` });
const bearer = (token: string) => ({ authorization: `Bearer ${token}` });

/** Makes `localpart` through the admin API and signs it in; its user id and access token. */
async function person(request: APIRequestContext, localpart: string) {
  const password = `hunter2-${localpart}-long-enough`;
  const created = await request.post("/api/v1/users", {
    headers: { ...admin(), "idempotency-key": `profile-${localpart}` },
    data: { localpart, password },
  });
  expect(created.ok(), await created.text()).toBe(true);
  const userId = ((await created.json()) as { user_id: string }).user_id;
  const login = await request.post("/_matrix/client/v3/login", {
    data: {
      type: "m.login.password",
      identifier: { type: "m.id.user", user: localpart },
      password,
    },
  });
  expect(login.ok(), await login.text()).toBe(true);
  return { userId, token: ((await login.json()) as { access_token: string }).access_token };
}

test.describe("How a user appears, against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed",
  );

  test("a name set in place reaches their profile and their rooms", async ({ page, request }) => {
    test.setTimeout(180_000);
    const subject = await person(request, `prof-${run}`);
    const friend = await person(request, `friend-${run}`);
    const room = await request.post("/_matrix/client/v3/createRoom", {
      headers: bearer(subject.token),
      data: { preset: "public_chat", name: `Profile ${run}` },
    });
    expect(room.ok(), await room.text()).toBe(true);
    const roomId = ((await room.json()) as { room_id: string }).room_id;
    const joined = await request.post(`/_matrix/client/v3/join/${encodeURIComponent(roomId)}`, {
      headers: bearer(friend.token),
      data: {},
    });
    expect(joined.ok(), await joined.text()).toBe(true);

    await page.goto("/");
    await page.getByRole("tab", { name: "Access token" }).click();
    await page.getByLabel("Access token").fill(adminToken!);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();
    await page.goto(`/admin/users/${encodeURIComponent(subject.userId)}`);

    const panel = page.locator("section", {
      has: page.getByRole("heading", { name: "How they appear" }),
    });
    const name = `Profiled ${run}`;
    await panel.getByLabel(/^Display name/).fill(name);
    await panel.getByLabel(/^Avatar URL/).fill(`mxc://example.org/${run}`);
    await panel.getByRole("combobox", { name: /Kind of account/ }).click();
    await page.getByRole("option", { name: "Bot" }).click();
    await shot(page, "editing", false);
    await panel.getByRole("button", { name: "Save profile" }).click();
    await expect(page.getByRole("heading", { name })).toBeVisible();

    // Their profile, as any client reads it.
    const profile = await request.get(
      `/_matrix/client/v3/profile/${encodeURIComponent(subject.userId)}`,
    );
    expect(await profile.json()).toMatchObject({
      displayname: name,
      avatar_url: `mxc://example.org/${run}`,
    });
    // Their membership in the shared room, as the friend reads it (re-sent asynchronously).
    await expect
      .poll(
        async () => {
          const member = await request.get(
            `/_matrix/client/v3/rooms/${encodeURIComponent(roomId)}/state/m.room.member/${encodeURIComponent(subject.userId)}`,
            { headers: bearer(friend.token) },
          );
          return ((await member.json()) as { displayname?: string }).displayname;
        },
        { timeout: 15_000 },
      )
      .toBe(name);
    const user = await request.get(`/api/v1/users/${encodeURIComponent(subject.userId)}`, {
      headers: admin(),
    });
    expect(((await user.json()) as { user_type: string | null }).user_type).toBe("bot");
    await shot(page, "saved", false);

    // ---- the create flow checks the name as it is typed ----
    await page.goto("/admin/users");
    await page.getByRole("button", { name: "Add user" }).click();
    const dialog = page.getByRole("dialog", { name: "Add a user" });
    const localpart = subject.userId.slice(1).split(":")[0]!;
    await dialog.getByLabel(/^Username/).fill(localpart.toUpperCase());
    await expect(dialog.getByText(/is taken\.$/)).toBeVisible();
    await dialog.getByLabel(/^Username/).fill(`free-${run}`);
    await expect(dialog.getByText(/is free\.$/)).toBeVisible();
    await dialog.getByLabel(/^Username/).fill("not a name");
    await expect(dialog.getByText(/^"not a name" cannot be a username/)).toBeVisible();
    await shot(page, "availability", false);
  });
});

async function shot(page: import("@playwright/test").Page, name: string, fullPage = true) {
  await settle(page);
  await page.screenshot({ path: `${SHOTS}/users-profile-${name}-real.png`, fullPage });
}
