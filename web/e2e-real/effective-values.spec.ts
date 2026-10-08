import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { SHOTS } from "./screenshots";
import { settle } from "./settle";

/**
 * Status 16's items 7 and 9 against a real `hs serve`, nothing mocked:
 *
 * - a user's message rate limit shows the server-wide limit the override replaces, read from
 *   the server's own `rate_limits` (`GET /users/{id}/rate-limit`'s `server_wide`, OpenAPI
 *   0.1.12), follows a change to it, and says what clearing the override does;
 * - an offering's settings dialog says what saving each section does to the bridges people
 *   already have, for a bridge that runs elsewhere;
 * - `auth.password.enabled` off: the sign-in page says password sign-in is turned off, in place
 *   of a bare refusal; and `server.admin_contact` is `/.well-known/matrix/support`.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts). Names carry a
 * per-run suffix; every setting changed is put back.
 */
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const run = Date.now().toString(36);

const authed = () => ({ authorization: `Bearer ${adminToken}` });

async function signIn(page: Page) {
  await page.goto("/");
  await page.getByRole("tab", { name: "Access token" }).click();
  await page.getByLabel("Access token").fill(adminToken!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();
  await expect(page.getByRole("banner")).toBeVisible();
}

async function makeUser(request: APIRequestContext, localpart: string) {
  const created = await request.post("/api/v1/users", {
    headers: { ...authed(), "idempotency-key": `effective-${localpart}` },
    data: { localpart, password: `hunter2-${localpart}-long-enough` },
  });
  expect(created.ok(), await created.text()).toBe(true);
  return (await created.json()) as { user_id: string };
}

async function patchConfig(request: APIRequestContext, section: string, patch: unknown) {
  const response = await request.patch(`/api/v1/config/${section}`, {
    headers: authed(),
    data: patch,
  });
  expect(response.ok(), await response.text()).toBe(true);
}

test.describe("Effective values and settings with a reader, against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN not set",
  );

  test("a user's rate limit shows the server-wide limit it replaces", async ({ page, request }) => {
    const { user_id } = await makeUser(request, `limits-${run}`);
    const before = await request.get("/api/v1/config/rate_limits", { headers: authed() });
    const message = ((await before.json()) as { values: { message: unknown } }).values.message;
    await patchConfig(request, "rate_limits", { message: { per_second: 0.4, burst_count: 12 } });
    try {
      const answer = await request.get(`/api/v1/users/${encodeURIComponent(user_id)}/rate-limit`, {
        headers: authed(),
      });
      expect(await answer.json()).toEqual({
        server_wide: {
          enabled: true,
          message: { per_second: 0.4, burst_count: 12 },
          admin_redaction: expect.objectContaining({}),
        },
      });

      await signIn(page);
      await page.goto(`/admin/users/${encodeURIComponent(user_id)}`);
      const moderation = page.getByRole("region", { name: "Moderation" });
      const serverWide = moderation.getByTestId("rate-limit-server-wide");
      await expect(serverWide).toContainText(
        "Server-wide limit: 0.4 messages a second, bursts of 12.",
      );
      await expect(
        moderation.getByText("0 exempts them from the limit. Server-wide: 0.4."),
      ).toBeVisible();

      await moderation.getByLabel("Messages per second").fill("0");
      await moderation.getByRole("button", { name: "Save limit" }).click();
      await expect(moderation.getByText("Override: Exempt from message rate limits")).toBeVisible();
      await expect(serverWide).toContainText(
        "This override replaces it for them; clearing the override puts them back on it.",
      );
      await settle(page);
      await moderation.screenshot({ path: `${SHOTS}/effective-values-rate-limit-real.png` });

      await moderation.getByRole("button", { name: "Clear override" }).click();
      await expect(
        moderation.getByText("No override: the server's own limits apply."),
      ).toBeVisible();
      const cleared = await request.get(`/api/v1/users/${encodeURIComponent(user_id)}/rate-limit`, {
        headers: authed(),
      });
      expect((await cleared.json()).messages_per_second).toBeUndefined();
    } finally {
      await patchConfig(request, "rate_limits", { message });
    }
  });

  test("the offering settings say what saving does to bridges people already have", async ({
    page,
    request,
  }) => {
    const type = "mautrix-signal";
    const { user_id } = await makeUser(request, `bridged-${run}`);
    const put = await request.put(`/api/v1/bridge-offerings/${type}`, {
      headers: authed(),
      data: {
        enabled: true,
        runtime: "elsewhere",
        access: { all_local_users: true },
        options: { encryption: true, double_puppeting: true, backfill: false },
      },
    });
    expect(put.ok(), await put.text()).toBe(true);
    try {
      const instance = await request.put(
        `/api/v1/bridge-offerings/${type}/instances/${encodeURIComponent(user_id)}`,
        { headers: authed() },
      );
      expect(instance.ok(), await instance.text()).toBe(true);

      await signIn(page);
      await page.goto(`/admin/bridges/offerings/${type}`);
      await page.getByRole("button", { name: "Edit settings" }).click();
      const dialog = page.getByRole("dialog", { name: /settings$/ });
      await expect(dialog.getByTestId("applies-access")).toContainText(
        "Applies to people who ask from now on.",
      );
      await expect(dialog.getByTestId("applies-options")).toContainText(
        "The 1 bridge people already have run elsewhere: each keeps its old settings until its files are downloaded again",
      );
      await settle(page);
      await page.screenshot({ path: `${SHOTS}/effective-values-offering-settings-real.png` });
      await dialog.getByRole("button", { name: "Cancel" }).click();
    } finally {
      const removed = await request.delete(
        `/api/v1/bridge-offerings/${type}?remove_instances=true`,
        {
          headers: authed(),
        },
      );
      expect(removed.ok(), await removed.text()).toBe(true);
    }
  });

  test("password sign-in turned off is said so; the admin contact is the support document", async ({
    page,
    request,
  }) => {
    const { user_id } = await makeUser(request, `pw-${run}`);
    await patchConfig(request, "auth", { password: { enabled: false } });
    try {
      const flows = await (await request.get("/_matrix/client/v3/login")).json();
      expect(flows.flows.map((f: { type: string }) => f.type)).not.toContain("m.login.password");

      await page.goto("/");
      await page.getByRole("tab", { name: "Username & password" }).click();
      await page.getByLabel("Username").fill(user_id);
      await page.getByLabel("Password").fill(`hunter2-pw-${run}-long-enough`);
      await page.getByRole("button", { name: "Sign in" }).click();
      await expect(page.getByText(/This server has password sign-in turned off/)).toBeVisible();
      await settle(page);
      await page.screenshot({ path: `${SHOTS}/effective-values-password-off-real.png` });
    } finally {
      await patchConfig(request, "auth", { password: { enabled: null } });
    }

    await patchConfig(request, "server", { admin_contact: `mailto:abuse-${run}@example.org` });
    try {
      const support = await request.get(
        `${process.env.HS_REAL_SERVER_URL}/.well-known/matrix/support`,
      );
      expect(support.status()).toBe(200);
      expect(await support.json()).toEqual({
        contacts: [{ role: "m.role.admin", email_address: `abuse-${run}@example.org` }],
      });
      // The Configuration page explains the setting from the server's own schema.
      await signIn(page);
      await page.goto("/admin/configuration/server");
      await expect(page.locator("#setting-admin_contact")).toContainText(
        "/.well-known/matrix/support",
      );
      await expect(page.locator("#setting-report_stats")).toHaveCount(0);
    } finally {
      await patchConfig(request, "server", { admin_contact: null });
    }
  });
});
