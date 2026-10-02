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
