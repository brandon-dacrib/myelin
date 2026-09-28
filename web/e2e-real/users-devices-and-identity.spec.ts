import { test, expect, type APIRequestContext, type Page } from "@playwright/test";
import { settle } from "./settle";

/**
 * The devices-and-identity controls on a user's page, against a real `hs serve`, nothing
 * mocked. Each control is checked by what it does to the person, through the client-server
 * API, not only by what the page shows:
 *
 * - Rename a device: the person's own device list shows the new name.
 * - Select a device and sign it out: its token stops working and `/keys/query` stops serving
 *   its keys; the other device is untouched.
 * - Add an email address: the person can sign in with it. Remove it: they cannot.
 * - Link an upstream identity: `users.lookup` finds the account by it.
 * - Switch an experimental feature: the admin API reports it on.
 * - A pusher and account data the person's client stored are listed on the page.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts). Names carry
 * a per-run suffix so a rerun does not collide. Screenshots go to
 * `docs/design/screenshots/users-identity-*-real.png`.
 */
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const SHOTS = "../docs/design/screenshots";
const run = Date.now().toString(36);

async function shot(page: Page, name: string, fullPage = true) {
  await settle(page);
  await page.screenshot({ path: `${SHOTS}/users-identity-${name}-real.png`, fullPage });
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

/** Signs `localpart` in on `deviceId`; the session's access token. */
async function login(
  request: APIRequestContext,
  identifier: Record<string, string>,
  password: string,
  deviceId?: string,
) {
  return request.post("/_matrix/client/v3/login", {
    data: {
      type: "m.login.password",
      identifier,
      password,
      ...(deviceId
        ? { device_id: deviceId, initial_device_display_name: `${deviceId} browser` }
        : {}),
    },
  });
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

async function keyedDevices(request: APIRequestContext, token: string, userId: string) {
  const queried = await request.post("/_matrix/client/v3/keys/query", {
    headers: bearer(token),
    data: { device_keys: { [userId]: [] } },
  });
  const body = (await queried.json()) as { device_keys: Record<string, Record<string, unknown>> };
  return Object.keys(body.device_keys[userId] ?? {}).sort();
}

test.describe("A user's devices and identity against the real server", () => {
  test.skip(
    !process.env.HS_REAL_SERVER_URL || !adminToken,
    "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed",
  );

  test("every control changes what the person experiences", async ({ page, request }) => {
    test.setTimeout(180_000);
    const localpart = `ident-${run}`;
    const password = `hunter2-${localpart}-long-enough`;
    const created = await request.post("/api/v1/users", {
      headers: { ...admin(), "idempotency-key": `e2e-${localpart}` },
      data: { localpart, password },
    });
    expect(created.ok(), await created.text()).toBe(true);
    const userId = ((await created.json()) as { user_id: string }).user_id;

    const phoneLogin = await login(
      request,
      { type: "m.id.user", user: localpart },
      password,
      "PHONE",
    );
    expect(phoneLogin.ok(), await phoneLogin.text()).toBe(true);
    const phone = ((await phoneLogin.json()) as { access_token: string }).access_token;
    const laptopLogin = await login(
      request,
      { type: "m.id.user", user: localpart },
      password,
      "LAPTOP",
    );
    const laptop = ((await laptopLogin.json()) as { access_token: string }).access_token;
    await uploadKeys(request, phone, userId, "PHONE");
    await uploadKeys(request, laptop, userId, "LAPTOP");
    expect(await keyedDevices(request, phone, userId)).toEqual(["LAPTOP", "PHONE"]);

    // What their clients store: a pusher and a piece of account data.
    const pusher = await request.post("/_matrix/client/v3/pushers/set", {
      headers: bearer(phone),
      data: {
        pushkey: `push-${run}`,
        kind: "http",
        app_id: "im.example.app",
        app_display_name: "Example",
        device_display_name: "Their phone",
        lang: "en",
        data: { url: "https://push.example.org/_matrix/push/v1/notify" },
      },
    });
    expect(pusher.ok(), await pusher.text()).toBe(true);
    const data = await request.put(
      `/_matrix/client/v3/user/${encodeURIComponent(userId)}/account_data/org.example.theme`,
      { headers: bearer(phone), data: { dark: true } },
    );
    expect(data.ok(), await data.text()).toBe(true);

    await signIn(page);
    await page.goto(`/admin/users/${encodeURIComponent(userId)}`);
    await expect(page.getByRole("heading", { name: userId })).toBeVisible();
    await expect(page.getByText("Their phone")).toBeVisible();
    await expect(page.getByText("org.example.theme")).toBeVisible();

    // ---- rename the laptop ----
    await page.getByRole("button", { name: "Rename LAPTOP browser" }).click();
    const rename = page.getByRole("dialog", { name: "Rename LAPTOP" });
    await rename.getByLabel("Device name").fill("Lost laptop");
    await shot(page, "rename", false);
    await rename.getByRole("button", { name: "Save name" }).click();
    await expect(page.getByText("Lost laptop")).toBeVisible();
    const own = await request.get("/_matrix/client/v3/devices/LAPTOP", { headers: bearer(phone) });
    expect(((await own.json()) as { display_name: string }).display_name).toBe("Lost laptop");

    // ---- sign the laptop out, alone ----
    await page.getByRole("checkbox", { name: "Select Lost laptop" }).check();
    await page.getByRole("button", { name: "Sign out selected (1)" }).click();
    const confirm = page.getByRole("dialog", { name: "Sign out 1 device?" });
    await confirm.getByRole("button", { name: "Sign out" }).click();
    await expect(page.getByText("Lost laptop")).toBeHidden();
    const whoami = await request.get("/_matrix/client/v3/account/whoami", {
      headers: bearer(laptop),
    });
    expect(whoami.status()).toBe(401);
    expect(await keyedDevices(request, phone, userId)).toEqual(["PHONE"]);

    // ---- an email address signs them in ----
    const email = `${localpart}@example.org`;
    const threepids = page.locator("section", {
      has: page.getByRole("heading", { name: "Email and phone" }),
    });
    await threepids.getByLabel("Email address").fill(email.toUpperCase());
    await threepids.getByRole("button", { name: "Add" }).click();
    await expect(threepids.getByText(email)).toBeVisible();
    const byEmail = await login(
      request,
      { type: "m.id.thirdparty", medium: "email", address: email },
      password,
    );
    expect(byEmail.status(), await byEmail.text()).toBe(200);

    // ---- a linked identity finds them ----
    const identities = page.locator("section", {
      has: page.getByRole("heading", { name: "Linked identities" }),
    });
    await identities.getByLabel("Provider", { exact: true }).fill("oidc-corp");
    await identities.getByLabel("Subject at the provider").fill(`sub-${run}`);
    await identities.getByRole("button", { name: "Link" }).click();
    await expect(identities.getByText(`sub-${run}`)).toBeVisible();
    const found = await request.get(
      `/api/v1/users/lookup?provider=oidc-corp&external_id=sub-${run}`,
      { headers: admin() },
    );
    expect(((await found.json()) as { user_id: string }).user_id).toBe(userId);

    // ---- an experimental feature ----
    const remote = page.getByRole("switch", { name: /Remote push toggles/ });
    await remote.click();
    await expect(remote).toBeChecked();
    const features = await request.get(
      `/api/v1/users/${encodeURIComponent(userId)}/experimental-features`,
      { headers: admin() },
    );
    expect(((await features.json()) as Record<string, boolean>).msc3881).toBe(true);
    await shot(page, "page");

    // ---- removing the address stops it signing them in ----
    await threepids.getByRole("button", { name: `Remove ${email}` }).click();
    await page
      .getByRole("dialog", { name: `Remove ${email}?` })
      .getByRole("button", { name: "Remove" })
      .click();
    await expect(threepids.getByText(email)).toBeHidden();
    const refused = await login(
      request,
      { type: "m.id.thirdparty", medium: "email", address: email },
      password,
    );
    expect(refused.status()).toBe(403);

    // Every change is on the record.
    for (const action of [
      "users.devices.update",
      "users.devices.bulk_delete",
      "users.threepids.add",
      "users.external_ids.add",
      "users.experimental_features.put",
      "users.threepids.remove",
    ]) {
      const audit = await request.get(`/api/v1/audit-log?action=${action}`, { headers: admin() });
      const items = ((await audit.json()) as { items: { target: { id: string } }[] }).items;
      expect(items[0]?.target.id, action).toBe(userId);
    }
  });
});
