import { test, expect, type Page } from "@playwright/test";
import { settle } from "./settle";

/**
 * The Configuration page against a real `hs serve`, nothing mocked, for the three things the
 * mock cannot prove about the real server's schema and store:
 *
 * - `media.scanning.icap.preview`, an externally tagged enum (`"negotiate"`, `"off"` or
 *   `{"bytes": N}`), is a choice with a number beneath it, and what it saves is what the server
 *   stores (queue item 2b).
 * - Bootstrap settings (decision 0010) are shown, marked "Set at install", and never offered for
 *   edit: `listeners` as a whole section, `server.signing_key_path` inside an administered one
 *   (queue item 2c).
 * - A hidden secret inside a list entry survives saving the list (RFC 0020): two OIDC providers
 *   with secrets are stored through the API, one is renamed through the page, and both still
 *   have their secret afterwards.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts). Screenshots go
 * to `test-results/real-configuration-*.png`.
 */
const server = process.env.HS_REAL_SERVER_URL;
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;

async function api(method: string, path: string, body?: unknown) {
  const response = await fetch(`${server}/api/v1${path}`, {
    method,
    headers: {
      authorization: `Bearer ${adminToken}`,
      ...(body === undefined ? {} : { "content-type": "application/json" }),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, json: text ? JSON.parse(text) : null };
}

async function signIn(page: Page) {
  await page.goto("/");
  await page.getByRole("tab", { name: "Access token" }).click();
  await page.getByLabel("Access token").fill(adminToken!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();
}

async function shot(page: Page, name: string) {
  await settle(page);
  await page.screenshot({ path: `test-results/real-configuration-${name}.png`, fullPage: true });
}

async function saveSection(page: Page, label: string) {
  await page.getByRole("button", { name: "Review and save" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Save changes" }).click();
  await expect(page.getByText(`${label} saved`, { exact: true })).toBeVisible();
}

test.describe("configuration against the real server", () => {
  test.skip(!server || !adminToken, "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed");

  test("the ICAP preview mode is a choice, and a forced size is stored as the server's shape", async ({
    page,
  }) => {
    test.setTimeout(90_000);
    // Start from no ICAP provider at all, whatever an earlier run left behind.
    expect((await api("PATCH", "/config/media", { scanning: { icap: null } })).status).toBe(200);
    await signIn(page);
    await page.goto("/admin/configuration/media");
    const preview = page.getByRole("group", { name: "Preview", exact: true });
    await expect(preview).toBeVisible();
    await expect(page.locator("textarea")).toHaveCount(0);
    // With no ICAP provider configured at all (`media.scanning.icap` is null), so
    // nothing is chosen yet; the provider's required settings are filled in alongside.
    await expect(preview.getByRole("combobox")).toHaveText(/Choose one/);
    await page.locator("#setting-scanning-icap-host").getByRole("textbox").fill("icap.test");
    await page.locator("#setting-scanning-icap-service").getByRole("textbox").fill("avscan");

    await preview.getByRole("combobox").click();
    await page.getByRole("option", { name: "Bytes" }).click();
    await preview.getByLabel(/^Bytes/).fill("4096");
    await preview.getByLabel(/^Bytes/).blur();
    await shot(page, "preview-bytes");
    await saveSection(page, "Media");

    const media = await api("GET", "/config/media");
    expect(media.status).toBe(200);
    expect(media.json.values.scanning.icap.preview).toEqual({ bytes: 4096 });

    // And back to a bare name.
    await page.reload();
    const again = page.getByRole("group", { name: "Preview", exact: true });
    await expect(again.getByRole("combobox")).toHaveText(/Bytes/);
    await expect(again.getByLabel(/^Bytes/)).toHaveValue("4096");
    await again.getByRole("combobox").click();
    await page.getByRole("option", { name: "Off" }).click();
    await saveSection(page, "Media");
    expect((await api("GET", "/config/media")).json.values.scanning.icap.preview).toBe("off");

    // Tidy up: an ICAP provider that does not exist would be asked about every upload.
    expect((await api("PATCH", "/config/media", { scanning: { icap: null } })).status).toBe(200);
  });

  test("bootstrap settings are shown as set at install, and never offered for edit", async ({
    page,
  }) => {
    test.setTimeout(60_000);
    const schema = await api("GET", "/config/schema");
    const listeners = schema.json.sections.find((s: { name: string }) => s.name === "listeners");
    expect(listeners.bootstrap).toBe(true);
    const signingKey = schema.json.settings.find(
      (s: { pointer: string }) => s.pointer === "/server/signing_key_path",
    );
    expect(signingKey).toMatchObject({ bootstrap: true, editable: false });

    await signIn(page);
    await page.goto("/admin/configuration");
    const card = page.getByRole("link", { name: "Listeners" }).locator("xpath=ancestor::li");
    await expect(card.getByText("Bootstrap only")).toBeVisible();

    await page.goto("/admin/configuration/listeners");
    await expect(page.getByText("This section cannot be stored in the database")).toBeVisible();
    const listenerRow = page.locator("#setting-listeners");
    await expect(listenerRow.getByText("Set at install", { exact: true })).toBeVisible();
    await expect(listenerRow.getByRole("button", { name: "Add listener" })).toHaveCount(0);
    await shot(page, "listeners");

    await page.goto("/admin/configuration/server");
    const keyRow = page.locator("#setting-signing_key_path");
    await expect(keyRow.getByText("Set at install", { exact: true })).toBeVisible();
    await expect(keyRow).toContainText(
      "the bootstrap file, an HS__ environment variable or the Helm values",
    );
    await expect(keyRow.getByRole("textbox")).toHaveCount(0);
    await shot(page, "server-bootstrap-setting");

    // The server refuses the same change outright, whatever a client sends.
    const refused = await api("PATCH", "/config/listeners", { listeners: [] });
    expect(refused.status).toBe(409);
  });

  test("a hidden secret inside a list entry survives renaming that entry (RFC 0020)", async ({
    page,
  }) => {
    test.setTimeout(90_000);
    const seeded = await api("PATCH", "/config/auth", {
      oidc_providers: [
        {
          idp_id: "alpha",
          idp_name: "Alpha",
          issuer: "https://alpha.example",
          client_id: "alpha-client",
          client_secret: "alpha-secret",
        },
        {
          idp_id: "beta",
          idp_name: "Beta",
          issuer: "https://beta.example",
          client_id: "beta-client",
          client_secret: "beta-secret",
        },
      ],
    });
    expect(seeded.status, JSON.stringify(seeded.json)).toBe(200);

    await signIn(page);
    await page.goto("/admin/configuration/auth");
    const providers = page.getByRole("group", { name: "OIDC providers", exact: true });
    await expect(providers).toBeVisible();
    // Move the second above the first (its secret must follow it), then rename the first.
    await providers.getByRole("button", { name: "Move OIDC provider 2 up" }).click();
    const alpha = page.getByRole("group", { name: /^OIDC provider 2/ });
    await alpha.getByLabel(/^IdP name/).fill("Alpha, renamed");
    await alpha.getByLabel(/^IdP name/).blur();
    await shot(page, "oidc-reordered");
    await saveSection(page, "Auth");

    const auth = await api("GET", "/config/auth");
    const saved = auth.json.values.oidc_providers;
    expect(saved.map((p: { idp_id: string }) => p.idp_id)).toEqual(["beta", "alpha"]);
    expect(saved[1].idp_name).toBe("Alpha, renamed");
    expect(saved[0].client_secret).toEqual({ $secret: true });
    expect(saved[1].client_secret).toEqual({ $secret: true });

    // Tidy up: the server answers this test's providers on its login page otherwise.
    expect((await api("PATCH", "/config/auth", { oidc_providers: null })).status).toBe(200);
  });

  test("a section's history names the setting that changed, and a revert puts it back", async ({
    page,
  }) => {
    expect((await api("PATCH", "/config/federation", { client_timeout: "61s" })).status).toBe(200);
    const changed = await api("PATCH", "/config/federation", { client_timeout: "62s" });
    expect(changed.status).toBe(200);
    const revision: number = changed.json.revision;

    await signIn(page);
    await page.goto("/admin/configuration/federation");
    const history = page.getByRole("region", { name: "Change history" });
    const latest = history.getByRole("listitem").filter({ hasText: `revision ${revision}` });
    await expect(latest.getByTitle("federation.client_timeout")).toContainText(
      /Client timeout:\s*61s\s*to\s*62s/,
    );
    await shot(page, "history");

    await latest.getByRole("button", { name: `Revert revision ${revision}` }).click();
    const dialog = page.getByRole("dialog", { name: `Revert revision ${revision}?` });
    await expect(dialog.getByTitle("federation.client_timeout")).toContainText(/62s\s*to\s*61s/);
    await shot(page, "revert-dialog");
    await dialog.getByRole("button", { name: "Revert", exact: true }).click();
    await expect(page.getByText(`Revision ${revision} reverted`, { exact: true })).toBeVisible();
    await expect(history.getByText(`Reverts revision ${revision}`)).toBeVisible();
    await shot(page, "reverted");

    const federation = await api("GET", "/config/federation");
    expect(federation.json.values.client_timeout).toBe("61s");
    expect(federation.json.history[0].reverts).toBe(revision);

    // Tidy up: back to the default.
    expect((await api("PATCH", "/config/federation", { client_timeout: null })).status).toBe(200);
  });
});
