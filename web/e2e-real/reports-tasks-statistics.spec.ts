import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { settle } from "./settle";

/**
 * The Reports, Tasks and Statistics pages against a real `hs serve`, nothing mocked.
 *
 * - Reports: two people are made through the admin API; one sends a message in a public room,
 *   the other reports the message and then its sender. The queue lists both; the message's
 *   report shows the message as the server holds it (`Report.event`: `type`, `sender`,
 *   `origin_server_ts`, `content`, `redacted`), the sender's other report, and is decided from
 *   the page.
 * - Tasks: a bridge's replay (recorded finished; its `resource` is `{type: appservice, id}`),
 *   a key refetch for a server that does not exist (a spawned task that fails), and a remote
 *   media purge (a spawned task that succeeds). The Tasks page lists them, the Bridges filter
 *   finds the replay, its page links to the bridge, and the failed one says why.
 * - Statistics: the "Now" tiles are the server's own counts, media included.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts), against a
 * server with no other reports, bridges or tasks named like these (names carry a per-run
 * suffix). Screenshots go to `docs/design/screenshots/rts-*-real.png`.
 */
const server = process.env.HS_REAL_SERVER_URL;
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const SHOTS = "../docs/design/screenshots";
const run = Date.now().toString(36);

async function shot(page: Page, name: string) {
  await settle(page);
  await page.screenshot({ path: `${SHOTS}/rts-${name}-real.png`, fullPage: true });
}

async function signIn(page: Page) {
  await page.goto("/");
  await page.getByRole("tab", { name: "Access token" }).click();
  await page.getByLabel("Access token").fill(adminToken!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();
}

const admin = () => ({ authorization: `Bearer ${adminToken}` });

/** Makes `localpart` through the admin API and signs in as them. */
async function makeUser(request: APIRequestContext, localpart: string) {
  const password = `hunter2-${localpart}-long-enough`;
  const created = await request.post(`${server}/api/v1/users`, {
    headers: { ...admin(), "idempotency-key": `e2e-${localpart}` },
    data: { localpart, password },
  });
  expect(created.ok(), await created.text()).toBe(true);
  const login = await request.post(`${server}/_matrix/client/v3/login`, {
    data: {
      type: "m.login.password",
      identifier: { type: "m.id.user", user: localpart },
      password,
    },
  });
  expect(login.ok(), await login.text()).toBe(true);
  return (await login.json()) as { access_token: string; user_id: string };
}

interface Task {
  id: string;
  action: string;
  status: string;
  resource?: { type: string; id: string };
}

/** The task `id`, once it has ended. */
async function settled(request: APIRequestContext, id: string): Promise<Task> {
  let task: Task | undefined;
  await expect
    .poll(
      async () => {
        const response = await request.get(`${server}/api/v1/tasks/${id}`, { headers: admin() });
        task = (await response.json()) as Task;
        return task.status;
      },
      { timeout: 30_000 },
    )
    .not.toMatch(/running|scheduled/);
  return task!;
}

test.describe("Reports, Tasks and Statistics against the real server", () => {
  test.skip(!server || !adminToken, "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed");

  test("a reported message, its sender's other report, and a decision", async ({
    page,
    request,
  }) => {
    test.setTimeout(120_000);
    const sender = await makeUser(request, `rts-sender-${run}`);
    const reporter = await makeUser(request, `rts-reporter-${run}`);
    const cs = (token: string) => ({ authorization: `Bearer ${token}` });
    const created = await request.post(`${server}/_matrix/client/v3/createRoom`, {
      headers: cs(sender.access_token),
      data: { preset: "public_chat", name: `Watches ${run}` },
    });
    expect(created.ok(), await created.text()).toBe(true);
    const roomId = ((await created.json()) as { room_id: string }).room_id;
    const joined = await request.post(
      `${server}/_matrix/client/v3/join/${encodeURIComponent(roomId)}`,
      { headers: cs(reporter.access_token), data: {} },
    );
    expect(joined.ok(), await joined.text()).toBe(true);
    const body = `buy cheap watches ${run}`;
    const sent = await request.put(
      `${server}/_matrix/client/v3/rooms/${encodeURIComponent(roomId)}/send/m.room.message/t-${run}`,
      { headers: cs(sender.access_token), data: { msgtype: "m.text", body } },
    );
    expect(sent.ok(), await sent.text()).toBe(true);
    const eventId = ((await sent.json()) as { event_id: string }).event_id;
    const reported = await request.post(
      `${server}/_matrix/client/v3/rooms/${encodeURIComponent(roomId)}/report/${encodeURIComponent(eventId)}`,
      { headers: cs(reporter.access_token), data: { reason: `spam ${run}`, score: -100 } },
    );
    expect(reported.ok(), await reported.text()).toBe(true);
    const aboutUser = await request.post(
      `${server}/_matrix/client/v3/users/${encodeURIComponent(sender.user_id)}/report`,
      { headers: cs(reporter.access_token), data: { reason: `spammer ${run}` } },
    );
    expect(aboutUser.ok(), await aboutUser.text()).toBe(true);

    await signIn(page);
    await page.goto(`/admin/reports?reported_user_id=${encodeURIComponent(sender.user_id)}`);
    await expect(page.getByRole("heading", { name: "Reports", level: 1 })).toBeVisible();
    const queue = page.getByRole("table", { name: "Reports" });
    // A header row and the two reports about the sender.
    await expect(queue.getByRole("row")).toHaveCount(3);
    await shot(page, "reports-queue");

    await queue.getByRole("link", { name: new RegExp(`^Message in (Watches ${run}|!)`) }).click();
    // The message as the server holds it: its text, its sender, its type.
    const message = page.getByRole("region", { name: "The reported message" });
    await expect(message.getByText(body)).toBeVisible();
    await expect(message.getByText(sender.user_id)).toBeVisible();
    await expect(message.getByText("m.room.message")).toBeVisible();
    await expect(page.getByText(`spam ${run}`)).toBeVisible();
    // The sender's other report, still open.
    const others = page.getByRole("region", { name: /Other reports about/ });
    await expect(others.getByRole("link", { name: `User ${sender.user_id}` })).toBeVisible();
    await expect(others.getByText("Open")).toBeVisible();
    await shot(page, "report-detail");

    await page.getByRole("radio", { name: /Warned the user/ }).check();
    await page.getByLabel("Note", { exact: true }).fill(`Warned ${run}.`);
    await page.getByRole("button", { name: "Resolve report" }).click();
    await expect(page.getByRole("heading", { name: "Decision" })).toBeVisible();
    await expect(page.getByText(`Warned ${run}.`)).toBeVisible();

    // The decision is the server's, not the page's.
    const listed = await request.get(
      `${server}/api/v1/reports?reported_user_id=${encodeURIComponent(sender.user_id)}`,
      { headers: admin() },
    );
    const items = ((await listed.json()) as { items: { kind: string; status: string }[] }).items;
    expect(items.find((r) => r.kind === "event")?.status).toBe("resolved");
    expect(items.find((r) => r.kind === "user")?.status).toBe("open");
  });

  test("a bridge's replay, a failed key refetch and a media purge on the Tasks page", async ({
    page,
    request,
  }) => {
    test.setTimeout(120_000);
    const bridge = `rts-bridge-${run}`;
    const registered = await request.post(`${server}/api/v1/appservices`, {
      headers: { ...admin(), "idempotency-key": `e2e-${bridge}` },
      data: {
        registration: {
          id: bridge,
          url: null,
          as_token: `as-${bridge}-token`,
          hs_token: `hs-${bridge}-token`,
          sender_localpart: `${bridge}-bot`,
          namespaces: { users: [], aliases: [], rooms: [] },
        },
      },
    });
    expect(registered.ok(), await registered.text()).toBe(true);
    const replay = await request.post(`${server}/api/v1/appservices/${bridge}/replay`, {
      headers: { ...admin(), "idempotency-key": `e2e-replay-${run}` },
      data: {},
    });
    expect(replay.status(), await replay.text()).toBe(202);
    const replayTask = (await replay.json()) as Task;
    // What a replay task's resource holds: the bridge, which the page links to.
    expect(replayTask.action).toBe("appservice.replay");
    expect(replayTask.resource).toEqual({ type: "appservice", id: bridge });

    const nowhere = `nowhere-${run}.invalid`;
    const refetch = await request.post(`${server}/api/v1/federation/keys/${nowhere}/refresh`, {
      headers: { ...admin(), "idempotency-key": `e2e-refetch-${run}` },
    });
    expect(refetch.status(), await refetch.text()).toBe(202);
    const refetchTask = await settled(request, ((await refetch.json()) as Task).id);
    expect(refetchTask.status).toBe("failed");
    const purge = await request.post(`${server}/api/v1/media/purge-remote-cache`, {
      headers: { ...admin(), "idempotency-key": `e2e-purge-${run}` },
      data: { server_name: nowhere },
    });
    expect(purge.status(), await purge.text()).toBe(202);
    expect((await settled(request, ((await purge.json()) as Task).id)).status).toBe("succeeded");

    await signIn(page);
    await page.goto("/admin/tasks");
    await expect(page.getByRole("heading", { name: "Tasks", level: 1 })).toBeVisible();
    const table = page.getByRole("table", { name: "Tasks" });
    await expect(table.getByRole("link", { name: "Refetch server keys" }).first()).toBeVisible();
    await expect(
      table.getByRole("link", { name: "Purge remote media cache" }).first(),
    ).toBeVisible();
    await shot(page, "tasks");

    // The Bridges filter finds the replay, under its own name.
    await page.getByRole("combobox", { name: "Kind" }).click();
    await page.getByRole("option", { name: "Bridges" }).click();
    await expect(page).toHaveURL(/action=appservice\./);
    await table.getByRole("link", { name: "Replay bridge transactions" }).first().click();
    await expect(page.getByRole("heading", { name: "Replay bridge transactions" })).toBeVisible();
    await expect(page.getByRole("link", { name: bridge })).toBeVisible();
    await shot(page, "task-replay");

    // The failed refetch says why, and links to the server it was about.
    await page.goto(`/admin/tasks/${refetchTask.id}`);
    await expect(page.getByRole("heading", { name: "Refetch server keys" })).toBeVisible();
    await expect(page.getByRole("alert")).toContainText(nowhere);
    await expect(page.getByRole("link", { name: nowhere })).toBeVisible();
    await shot(page, "task-failed");
  });

  test("the Statistics page's counts are the server's", async ({ page, request }) => {
    test.setTimeout(90_000);
    const response = await request.get(`${server}/api/v1/statistics/overview`, {
      headers: admin(),
    });
    expect(response.ok()).toBe(true);
    const counts = (await response.json()) as {
      users_count: number;
      rooms_count: number;
      media_count?: number;
    };
    expect(counts.media_count, "the overview counts media").toBeDefined();

    await signIn(page);
    await page.goto("/admin/statistics");
    await expect(page.getByRole("heading", { name: "Statistics", level: 1 })).toBeVisible();
    const now = page.getByRole("region", { name: "Now" });
    const tile = (label: string) => now.getByText(label, { exact: true }).locator("..");
    await expect(tile("Accounts")).toContainText(counts.users_count.toLocaleString("en-US"));
    await expect(tile("Rooms")).toContainText(counts.rooms_count.toLocaleString("en-US"));
    await expect(tile("Media files")).toContainText(counts.media_count!.toLocaleString("en-US"));
    await expect(tile("Media stored")).not.toContainText("—");
    await shot(page, "statistics");
  });
});
