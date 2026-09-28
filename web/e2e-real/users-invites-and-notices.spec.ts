import { test, expect, type APIRequestContext, type Page } from "@playwright/test";

/**
 * The Users page's invite link and a user's "Send notice", against a real `hs serve`, nothing
 * mocked. Both reuse Settings' flows (`CreateTokenDialog`, `SendNoticeDialog`); this proves
 * they work from where an administrator dealing with people reaches for them.
 *
 * - Users list: "Invite by link" makes a one-use registration token. The link opens the
 *   register page in a fresh, signed-out browser, where the invited person chooses their own
 *   username and password. The same link then says it no longer works, and the account shows
 *   up in the users list.
 * - A user's page: "Send notice" delivers a server notice. The recipient's own client sees an
 *   invitation to "Server Notices" from `@_server:<server>`.
 *
 * Ported from the superseded `worktree-agent-aafb071194d2144c6` branch's `admin-areas.spec.ts`
 * and fitted to main's dialogs. Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN`
 * (playwright.real.config.ts). Registration may be closed: the recipient is made through the
 * admin API. Names carry a per-run suffix so that a rerun does not collide. Screenshots go to
 * `docs/design/screenshots/users-*-real.png` as the record of the run.
 */
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const SHOTS = "../docs/design/screenshots";
const run = Date.now().toString(36);

/** The whole page, or just the viewport for a dialog (a full-page capture scrolls it away). */
async function shot(page: Page, name: string, fullPage = true) {
  await page.waitForLoadState("networkidle");
  await page.screenshot({ path: `${SHOTS}/users-${name}-real.png`, fullPage });
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
  return (await login.json()) as { access_token: string; user_id: string };
}

interface SyncInvites {
  rooms?: {
    invite?: Record<
      string,
      { invite_state: { events: { type: string; sender?: string; content: { name?: string } }[] } }
    >;
  };
}

test.describe("Users page invite links and notices against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed",
  );

  test("invite by link from the users list: the invited person registers themselves", async ({
    page,
    browser,
  }) => {
    test.setTimeout(120_000);
    await signIn(page);
    await page.goto("/admin/users");
    await expect(page.getByRole("heading", { name: "Users" })).toBeVisible();
    await page.getByRole("button", { name: "Invite by link" }).click();
    const dialog = page.getByRole("dialog", { name: "Create an invite link" });
    await expect(dialog).toBeVisible();
    await shot(page, "invite-dialog", false);
    await dialog.getByRole("button", { name: "Create invite link" }).click();

    const done = page.getByRole("dialog", { name: "Invite link ready" });
    const link = (await done.getByText(/\/admin\/register\?token=/).textContent())!.trim();
    await expect(done.getByText("Uses allowed")).toBeVisible();
    await shot(page, "invite-ready", false);
    await done.getByRole("button", { name: "Done" }).click();

    // The person, in a browser of their own: no session, just the link.
    const theirs = await browser.newContext();
    const their = await theirs.newPage();
    const localpart = `invited-${run}`;
    await their.goto(link);
    await expect(their.getByRole("heading", { name: "Create your account" })).toBeVisible();
    await their.getByLabel(/^Username/).fill(localpart);
    await their.getByLabel(/^Password/).fill("correct horse battery staple");
    await their.getByLabel(/^Confirm password/).fill("correct horse battery staple");
    await their.getByRole("button", { name: "Create account" }).click();
    await expect(their.getByRole("heading", { name: "Your account is ready" })).toBeVisible();
    await expect(their.getByText(`@${localpart}:`)).toBeVisible();
    await shot(their, "invite-registered", false);

    // The same link, again: one use, spent.
    await their.goto(link);
    await expect(their.getByText(/This invite link is no longer valid/)).toBeVisible();
    await theirs.close();

    // The administrator sees the account, made without ever seeing its password.
    await page.goto(`/admin/users?q=${localpart}`);
    await expect(page.getByRole("link", { name: new RegExp(`^@${localpart}:`) })).toBeVisible();
    await shot(page, "invite-list-after");
  });

  test("send notice from a user's page: it reaches them from the server", async ({
    page,
    request,
  }) => {
    test.setTimeout(120_000);
    const recipient = await makeUser(request, `noticed-${run}`);

    await signIn(page);
    await page.goto(`/admin/users/${encodeURIComponent(recipient.user_id)}`);
    await expect(page.getByRole("heading", { name: recipient.user_id })).toBeVisible();
    await page.getByRole("button", { name: "Send notice" }).click();
    const dialog = page.getByRole("dialog", { name: "Send a server notice" });
    await expect(dialog.getByText(recipient.user_id)).toBeVisible();
    await dialog
      .getByLabel(/^Message/)
      .fill("Maintenance tonight at 22:00; expect ten minutes down.");
    await shot(page, "notice-dialog", false);
    await dialog.getByRole("button", { name: "Send notice" }).click();
    await expect(dialog.getByText("Notice sent to 1 user.")).toBeVisible();
    await shot(page, "notice-sent", false);
    await dialog.getByRole("button", { name: "Done" }).click();

    // Their client sees an invitation to "Server Notices" from the server-notices user.
    let events: { type: string; sender?: string; content: { name?: string } }[] = [];
    await expect
      .poll(async () => {
        const synced = await request.get("/_matrix/client/v3/sync?timeout=0", {
          headers: { authorization: `Bearer ${recipient.access_token}` },
        });
        const body = (await synced.json()) as SyncInvites;
        events = Object.values(body.rooms?.invite ?? {})[0]?.invite_state.events ?? [];
        return events.length;
      })
      .toBeGreaterThan(0);
    expect(
      events.some((e) => e.type === "m.room.name" && e.content.name === "Server Notices"),
    ).toBe(true);
    expect(
      events.some((e) => e.type === "m.room.member" && e.sender?.startsWith("@_server:")),
    ).toBe(true);

    // And the send is on the record, attributed to whoever signed in.
    await page.goto("/admin/audit?action=server_notices.send");
    await expect(page.locator('table a[href*="/audit/"]').first()).toBeVisible();
    await shot(page, "notice-audit");
  });
});
