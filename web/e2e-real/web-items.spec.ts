import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { settle } from "./settle";

/**
 * The 2026-10-02 web items against a real `hs serve`, nothing mocked: editing an account
 * (administrator granted after creation; a field the server cannot change yet refused beside
 * the field in the server's words), editing and testing a bridge, the Overview's server health,
 * the exact user lookup and the live username check, and a room's lifecycle facts.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts). Names carry
 * a per-run suffix so a rerun does not collide.
 */
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const run = Date.now().toString(36);

async function signIn(page: Page) {
  await page.goto("/");
  await page.getByRole("tab", { name: "Access token" }).click();
  await page.getByLabel("Access token").fill(adminToken!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();
  await expect(page.getByRole("banner")).toBeVisible();
}

function authed() {
  return { authorization: `Bearer ${adminToken}` };
}

async function makeUser(request: APIRequestContext, localpart: string, displayName?: string) {
  const created = await request.post("/api/v1/users", {
    headers: { ...authed(), "idempotency-key": `web-items-${localpart}` },
    data: { localpart, password: `hunter2-${localpart}-long-enough`, display_name: displayName },
  });
  expect(created.ok(), await created.text()).toBe(true);
  return (await created.json()) as { user_id: string };
}

test.describe("Web items against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN not set",
  );

  test("edit account: administrator is granted after creation, and a refused field is explained", async ({
    page,
    request,
  }) => {
    const { user_id } = await makeUser(request, `edit-${run}`, "Edit Me");
    await signIn(page);
    await page.goto(`/admin/users/${encodeURIComponent(user_id)}`);
    await expect(page.getByRole("heading", { name: "Edit Me" })).toBeVisible();
    await expect(page.getByText("Admin", { exact: true })).toHaveCount(0);

    await page.getByRole("button", { name: "Edit", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "Edit account" });
    await dialog.getByRole("switch", { name: /Server administrator/ }).click();
    await dialog.getByRole("button", { name: "Save" }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByText("Admin", { exact: true })).toBeVisible();
    const after = await request.get(`/api/v1/users/${encodeURIComponent(user_id)}`, {
      headers: authed(),
    });
    expect(((await after.json()) as { admin: boolean }).admin).toBe(true);

    // A display name is a field this server cannot change yet: refused beside the field.
    await page.getByRole("button", { name: "Edit", exact: true }).click();
    await dialog.getByLabel(/^Display name/).fill("Edited Name");
    await dialog.getByRole("button", { name: "Save" }).click();
    await expect(dialog.getByRole("alert")).toContainText(
      "This server says: no data source can change this field yet.",
    );
    await settle(page);
    await page.screenshot({ path: "test-results/real-edit-account-refused.png" });
    await dialog.getByRole("button", { name: "Cancel" }).click();
    await expect(page.getByRole("heading", { name: "Edit Me" })).toBeVisible();
  });
});

test.describe("Bridge edit and test against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken || !process.env.HS_REAL_STUB_BRIDGE_URL,
    "HS_REAL_SERVER_URL, HS_REAL_ADMIN_TOKEN and HS_REAL_STUB_BRIDGE_URL (a server answering POST /_matrix/app/v1/ping with 200) not set",
  );

  test("test connection answers, the registration is edited, and a dead url does not answer", async ({
    page,
    request,
  }) => {
    const id = `wi-${run}`;
    // How the page names a bridge with no catalogue entry (`deriveDisplayName`).
    const name = id
      .split(/[-_]/)
      .map((part) => part.charAt(0).toUpperCase() + part.slice(1))
      .join(" ");
    const stubUrl = process.env.HS_REAL_STUB_BRIDGE_URL!;
    const created = await request.post("/api/v1/appservices", {
      headers: { ...authed(), "idempotency-key": `web-items-as-${run}` },
      data: {
        registration: {
          id,
          url: stubUrl,
          as_token: `as_${run}_0123456789abcdef`,
          hs_token: `hs_${run}_0123456789abcdef`,
          sender_localpart: `${id}bot`,
          rate_limited: false,
          namespaces: { users: [{ regex: `@${id}_.*:example\\.org`, exclusive: true }] },
        },
      },
    });
    expect(created.ok(), await created.text()).toBe(true);

    await signIn(page);
    await page.goto(`/admin/bridges/${id}`);
    await expect(page.getByRole("heading", { name })).toBeVisible();
    await expect(page.getByText(`@${id}_.*:example\\.org`)).toBeVisible();

    await page.getByRole("button", { name: "Test connection" }).click();
    await expect(page.getByText(`${name} answered`).first()).toBeVisible();
    await expect(page.getByText("Healthy", { exact: true }).first()).toBeVisible();

    await page.getByRole("button", { name: "Edit", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: `Edit ${name}` });
    const deadUrl = stubUrl.replace(/:(\d+)$/, (_m, port) => `:${Number(port) + 1}`);
    await dialog.getByLabel(/^URL/).fill(deadUrl);
    await dialog.getByRole("switch", { name: "Rate limited" }).click();
    await dialog.getByRole("button", { name: "Add rule" }).nth(1).click();
    await dialog.getByLabel("Alias namespaces pattern").fill(`#${id}_.*:example\\.org`);
    await dialog.getByRole("button", { name: "Save" }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByText(deadUrl)).toBeVisible();
    await expect(page.getByText("Yes", { exact: true })).toBeVisible();
    await expect(page.getByText(`#${id}_.*:example\\.org`)).toBeVisible();
    const after = await request.get(`/api/v1/appservices/${id}`, { headers: authed() });
    const body = (await after.json()) as {
      url: string;
      rate_limited: boolean;
      namespaces: { aliases: { regex: string; exclusive: boolean }[] };
    };
    expect(body.url).toBe(deadUrl);
    expect(body.rate_limited).toBe(true);
    expect(body.namespaces.aliases).toEqual([
      { regex: `#${id}_.*:example\\.org`, exclusive: true },
    ]);

    await page.getByRole("button", { name: "Test connection" }).click();
    await expect(page.getByText(`${name} did not answer`).first()).toBeVisible();
    await expect(page.getByText(/failed to connect to the appservice/)).toBeVisible();
    await settle(page);
    await page.screenshot({ path: "test-results/real-bridge-did-not-answer.png", fullPage: true });

    const deleted = await request.delete(`/api/v1/appservices/${id}`, { headers: authed() });
    expect(deleted.ok()).toBe(true);
  });
});

test.describe("Overview health against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN not set",
  );

  test("the server's own checks are on the Overview, in words", async ({ page, request }) => {
    const answer = await request.get("/api/v1/server/health", { headers: authed() });
    const health = (await answer.json()) as { status: string; checks: Record<string, string> };
    await signIn(page);
    await page.goto("/admin/");
    const card = page.getByRole("region", { name: "Health" });
    await expect(
      card.getByText(health.status === "ok" ? "Ok" : /Degraded|Down/).first(),
    ).toBeVisible();
    for (const key of Object.keys(health.checks)) {
      const label = { audit: "Audit log", events: "Event stream", users: "User directory" }[key];
      if (label) await expect(card.getByText(label, { exact: true })).toBeVisible();
    }
    await settle(page);
    await page.screenshot({ path: "test-results/real-overview-health.png" });
  });
});

test.describe("Exact lookup and the username check against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN not set",
  );

  test("an email the server holds opens the account; the username check says what it can", async ({
    page,
    request,
  }) => {
    const localpart = `find-${run}`;
    const { user_id } = await makeUser(request, localpart, "Find Me");
    const address = `${localpart}@example.org`;
    const added = await request.post(`/api/v1/users/${encodeURIComponent(user_id)}/threepids`, {
      headers: authed(),
      data: { medium: "email", address },
    });
    expect(added.ok(), await added.text()).toBe(true);

    await signIn(page);
    await page.goto("/admin/users");
    await page.getByText("Find by email, phone or sign-in provider").click();
    await page.getByLabel(/^Email address/).fill(address);
    await page.getByRole("button", { name: "Find the account" }).click();
    await expect(page.getByRole("heading", { name: "Find Me" })).toBeVisible();

    await page.goto("/admin/users");
    await page.getByText("Find by email, phone or sign-in provider").click();
    await page.getByLabel(/^Email address/).fill(`nobody-${run}@example.org`);
    await page.getByRole("button", { name: "Find the account" }).click();
    await expect(page.getByRole("status")).toHaveText(
      `No account has nobody-${run}@example.org as a verified email address.`,
    );

    // The username check: this server's directory answers 503, and the dialog says so.
    const availability = await request.get(`/api/v1/users/availability?localpart=${localpart}`, {
      headers: authed(),
    });
    await page.getByRole("button", { name: "Add user" }).click();
    const dialog = page.getByRole("dialog", { name: "Add a user" });
    await dialog.getByLabel(/^Username/).fill(localpart);
    if (availability.status() === 503) {
      await expect(
        dialog.getByText(/can’t check usernames in advance; a taken one is refused on Create/),
      ).toBeVisible();
    } else {
      await expect(dialog.getByText(`${user_id} is taken.`)).toBeVisible();
    }
    await settle(page);
    await page.screenshot({ path: "test-results/real-add-user-availability.png" });
  });
});

test.describe("Room lifecycle against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN not set",
  );

  test("an upgraded room links its successor, guests are named, and Block keeps the reason", async ({
    page,
    request,
  }) => {
    const localpart = `room-${run}`;
    await makeUser(request, localpart);
    const login = await request.post("/_matrix/client/v3/login", {
      data: {
        type: "m.login.password",
        identifier: { type: "m.id.user", user: localpart },
        password: `hunter2-${localpart}-long-enough`,
      },
    });
    expect(login.ok(), await login.text()).toBe(true);
    const { access_token } = (await login.json()) as { access_token: string };
    const client = { authorization: `Bearer ${access_token}` };
    const created = await request.post("/_matrix/client/v3/createRoom", {
      headers: client,
      data: { name: `Lifecycle ${run}`, preset: "public_chat", room_version: "10" },
    });
    expect(created.ok(), await created.text()).toBe(true);
    const { room_id } = (await created.json()) as { room_id: string };
    const guests = await request.put(
      `/_matrix/client/v3/rooms/${encodeURIComponent(room_id)}/state/m.room.guest_access`,
      { headers: client, data: { guest_access: "can_join" } },
    );
    expect(guests.ok(), await guests.text()).toBe(true);
    const upgraded = await request.post(
      `/_matrix/client/v3/rooms/${encodeURIComponent(room_id)}/upgrade`,
      { headers: client, data: { new_version: "11" } },
    );
    expect(upgraded.ok(), await upgraded.text()).toBe(true);
    const { replacement_room } = (await upgraded.json()) as { replacement_room: string };

    await signIn(page);
    await page.goto(`/admin/rooms/${encodeURIComponent(room_id)}`);
    await expect(page.getByRole("heading", { name: `Lifecycle ${run}`, level: 1 })).toBeVisible();
    await expect(page.getByText("Upgraded", { exact: true })).toBeVisible();
    await expect(page.getByText("Guests may join")).toBeVisible();
    await expect(page.getByRole("link", { name: replacement_room }).first()).toBeVisible();

    await page.getByRole("button", { name: "Block", exact: true }).click();
    const dialog = page.getByRole("dialog");
    await dialog.getByLabel(/^Reason/).fill(`Spam ring ${run}`);
    await dialog.getByRole("button", { name: "Block" }).click();
    await expect(page.getByText(`Blocked: Spam ring ${run}`)).toBeVisible();
    const after = await request.get(`/api/v1/rooms/${encodeURIComponent(room_id)}`, {
      headers: authed(),
    });
    const room = (await after.json()) as {
      blocked: boolean;
      blocked_reason: string | null;
      tombstoned: boolean;
      replacement_room_id: string | null;
    };
    expect(room.blocked).toBe(true);
    expect(room.blocked_reason).toBe(`Spam ring ${run}`);
    expect(room.tombstoned).toBe(true);
    expect(room.replacement_room_id).toBe(replacement_room);
    await settle(page);
    await page.screenshot({ path: "test-results/real-room-lifecycle.png", fullPage: true });

    await page.getByRole("link", { name: replacement_room }).first().click();
    await expect(page.getByRole("heading", { name: `Lifecycle ${run}`, level: 1 })).toBeVisible();
    await expect(page.getByText("Upgraded", { exact: true })).toHaveCount(0);
  });
});

test.describe("Wording against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN not set",
  );

  test("the sign-in page, the audit filter and a task's result read in words", async ({
    page,
    request,
  }) => {
    await page.goto("/");
    await expect(
      page.getByText(/any account that administers this server works here/),
    ).toBeVisible();
    await expect(page.getByText("is_admin")).toHaveCount(0);

    await signIn(page);
    await page.goto("/admin/audit");
    const action = page.getByLabel("Action", { exact: true });
    await expect(action).toHaveAttribute("placeholder", "Any action");
    await expect(page.getByText(/by the server's name for it; the list offers each/)).toBeVisible();
    const suggestions = await page.evaluate(() => {
      const input = document.querySelector<HTMLInputElement>('input[list$="-actions"]');
      const list = input?.list;
      return list ? Array.from(list.options).map((o) => [o.value, o.label]) : [];
    });
    expect(suggestions).toContainEqual(["users.update", "Updated user"]);
    expect(suggestions).toContainEqual(["appservices.ping", "Pinged bridge"]);
    await action.fill("users.update");
    await page.getByRole("button", { name: "Apply" }).click();
    await expect(page).toHaveURL(/action=users\.update/);
    await expect(page.getByText("Updated user").first()).toBeVisible();

    // A task with a result: redact everything a fresh user sent (nothing), through the API.
    const { user_id } = await makeUser(request, `task-${run}`);
    const started = await request.post(
      `/api/v1/users/${encodeURIComponent(user_id)}/redact-events`,
      { headers: { ...authed(), "idempotency-key": `web-items-task-${run}` }, data: {} },
    );
    expect(started.ok(), await started.text()).toBe(true);
    const task = (await started.json()) as { id: string };
    await page.goto(`/admin/tasks/${task.id}`);
    await expect(page.getByRole("heading", { name: "Result" })).toBeVisible({ timeout: 30_000 });
    const labels = await page.locator("dl dt").allTextContents();
    expect(labels.length).toBeGreaterThan(0);
    for (const label of labels) expect(label).toMatch(/^[A-Z][^_]*$/);
    await settle(page);
    await page.screenshot({ path: "test-results/real-task-result-words.png" });
  });
});
