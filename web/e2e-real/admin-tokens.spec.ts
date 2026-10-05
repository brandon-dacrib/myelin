import { test, expect, type Page } from "@playwright/test";
import { SHOTS } from "./screenshots";
import { settle } from "./settle";

/**
 * Admin tokens against a real `hs serve`, nothing mocked: the Settings page mints a
 * `bridges:read` token, the token is served `GET /api/v1/bridge-types` and refused
 * `GET /api/v1/users` with `403 insufficient-scope` naming `admin:read`, the page lists it with
 * its scope, and revoking it from the page makes its next request `401`. Needs
 * `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts). Screenshots go to
 * `admin-tokens-*-real.png` in `SHOTS` (`./screenshots.ts`) as the record of the run.
 */
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const run = Date.now().toString(36);

async function shot(page: Page, name: string, fullPage = true) {
  await settle(page);
  await page.screenshot({ path: `${SHOTS}/admin-tokens-${name}-real.png`, fullPage });
}

async function signIn(page: Page) {
  await page.goto("/");
  await page.getByRole("tab", { name: "Access token" }).click();
  await page.getByLabel("Access token").fill(adminToken!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();
}

test.describe("Admin tokens against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed",
  );

  test("a bridges:read token minted on the page works inside its scope and not outside", async ({
    page,
    request,
  }) => {
    test.setTimeout(120_000);
    await signIn(page);
    await page.goto("/admin/settings/admin-tokens");
    await expect(page.getByRole("heading", { name: "Admin tokens" })).toBeVisible();
    await shot(page, "list");

    const name = `bridge watcher ${run}`;
    await page.getByRole("button", { name: "Mint token" }).click();
    const dialog = page.getByRole("dialog", { name: "Mint an admin token" });
    await dialog.getByLabel(/^Name/).fill(name);
    await dialog.getByRole("checkbox", { name: /^admin:write/ }).uncheck();
    await dialog.getByRole("checkbox", { name: /^admin:read/ }).uncheck();
    await dialog.getByRole("checkbox", { name: /^bridges:read/ }).check();
    await shot(page, "mint-dialog", false);
    await dialog.getByRole("button", { name: "Mint token" }).click();

    const done = page.getByRole("dialog", { name: "Admin token ready" });
    const token = (await done.getByTestId("token").textContent())!.trim();
    expect(token).toMatch(/^hsa_/);
    await shot(page, "minted", false);
    await done.getByRole("button", { name: "Done" }).click();

    const row = page.getByRole("row", { name: new RegExp(name) });
    await expect(row.getByText("bridges:read")).toBeVisible();
    await shot(page, "listed");

    // Inside its scope.
    const types = await request.get("/api/v1/bridge-types", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(types.status(), await types.text()).toBe(200);
    // Outside it: the RFC's 403, naming the scope.
    const users = await request.get("/api/v1/users", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(users.status()).toBe(403);
    const problem = (await users.json()) as { type: string; required_scope: string };
    expect(problem.type).toBe("urn:hs:problem:insufficient-scope");
    expect(problem.required_scope).toBe("admin:read");
    // What it is.
    const me = await request.get("/api/v1/me", { headers: { authorization: `Bearer ${token}` } });
    expect((await me.json()).scopes).toEqual(["bridges:read"]);

    // Revoked from the page: its next request is 401.
    await row.getByRole("button", { name: `Revoke ${name}` }).click();
    await page.getByRole("button", { name: "Revoke token" }).click();
    await expect(page.getByRole("row", { name: new RegExp(name) })).toBeHidden();
    const after = await request.get("/api/v1/me", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(after.status()).toBe(401);
  });
});
