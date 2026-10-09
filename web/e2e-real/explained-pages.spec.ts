import { test, expect, type Page } from "@playwright/test";
import { settle } from "./settle";

/**
 * The owner's rule (2026-10-01): every page explains itself. Against a real `hs serve`, this
 * walks the pages the `agent/web-admin-ui` branch changed and checks they explain themselves
 * from the server's own answers -- when each setting applies (`ConfigSettingInfo.applies`), the
 * queue limit behind catch-up, the importer's streams and what a migration leaves behind -- and
 * that a save names what applied. Screenshots go to `test-results/real-explained-*.png`.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (see `real-server.spec.ts`).
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
  await page.screenshot({ path: `test-results/real-explained-${name}.png`, fullPage: false });
}

test.describe("pages that explain themselves, against the real server", () => {
  test.skip(!server || !adminToken, "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed");

  test("Configuration: every setting says when it applies, from the server's own answer", async ({
    page,
  }) => {
    test.setTimeout(90_000);
    const schema = await api("GET", "/config/schema");
    const queue = schema.json.settings.find(
      (s: { pointer: string }) => s.pointer === "/federation/max_queued_pdus_per_destination",
    );
    expect(queue).toMatchObject({ applies: "restart" });

    await signIn(page);
    await page.goto("/admin/configuration");
    await expect(page.getByText(/Every setting has a default the server works with/)).toBeVisible();
    // The three classes are explained once, behind a disclosure that starts closed.
    await expect(page.getByText(/Per replica \(file or environment\)/)).toBeHidden();
    await page.getByText("How a change takes effect").click();
    await expect(page.getByText(/Per replica \(file or environment\)/).first()).toBeVisible();
    await expect(page.getByRole("link", { name: "Sign-in and registration" })).toBeVisible();
    await shot(page, "configuration-index");

    await page.goto("/admin/configuration/federation");
    const legend = page.getByRole("region", { name: /^Most changes here/ });
    await expect(legend.getByText("Needs a restart", { exact: true })).toBeVisible();
    await expect(legend.getByText("Applies on save", { exact: true })).toBeVisible();
    const row = page.locator("#setting-max_queued_pdus_per_destination");
    await expect(row.getByText("Needs a restart", { exact: true })).toBeVisible();
    await expect(row).toContainText("Default: 10000");
    await expect(row).toContainText("caught up with the latest event of each room");
    const allowlist = page.locator("#setting-domain_allowlist");
    await expect(allowlist.getByText("Applies on save", { exact: true })).toBeVisible();
    await shot(page, "configuration-federation");

    await page.goto("/admin/configuration/rate_limits");
    const note = page.getByRole("region", { name: "How rate limits work" });
    await expect(note).toContainText("M_LIMIT_EXCEEDED");
    await expect(note).toContainText("each replica counts on its own");
    // The rewritten doc comments reach the page through the server's schema.
    await expect(page.getByText(/the guard against password guessing/).first()).toBeVisible();
    await shot(page, "configuration-rate-limits");

    // A save names what it applied and what waits.
    const before = (await api("GET", "/config/federation")).json.values;
    await page.goto("/admin/configuration/federation");
    const field = page.getByRole("textbox", { name: "Client timeout" });
    await field.fill(before.client_timeout === "31s" ? "32s" : "31s");
    await field.blur();
    await page.getByRole("button", { name: "Review and save" }).click();
    const dialog = page.getByRole("dialog");
    await expect(dialog.getByText("Needs a restart", { exact: true })).toBeVisible();
    await dialog.getByRole("button", { name: "Save changes" }).click();
    await expect(
      page.getByText("Stored, and waiting for the next restart: Client timeout.", { exact: true }),
    ).toBeVisible();
    await shot(page, "configuration-saved-restart");
    expect(
      (await api("PATCH", "/config/federation", { client_timeout: before.client_timeout })).status,
    ).toBe(200);
  });

  test("Federation: the list explains its statuses, with the server's queue limit", async ({
    page,
  }) => {
    await signIn(page);
    await page.goto("/admin/federation");
    await expect(page.getByRole("heading", { name: "Federation", level: 1 })).toBeVisible();
    await expect(page.getByText(/The other Matrix servers this one sends to/)).toBeVisible();
    await page.getByText("What the statuses mean").click();
    await expect(page.getByText(/longer than its queue holds \(10,000 events\)/)).toBeVisible();
    await shot(page, "federation");
  });

  test("Migration: every stream named in words, and what is not copied, before a start", async ({
    page,
  }) => {
    await signIn(page);
    await page.goto("/admin/migration");
    await expect(
      page.getByRole("heading", { name: "Migration from Synapse", level: 1 }),
    ).toBeVisible();
    const copied = page.getByRole("region", { name: "Copied, in this order" });
    const status = (await api("GET", "/migration")).json.status;
    if (status === "idle") {
      await expect(copied).toBeVisible();
    } else {
      await page.getByText("What is copied, and what is not").click();
    }
    await expect(copied.getByRole("listitem")).toHaveCount(19);
    await expect(copied.getByText("Device encryption keys")).toBeVisible();
    await expect(copied.getByText("Refresh tokens", { exact: true })).toBeVisible();
    await expect(copied.getByText("Other servers' media", { exact: true })).toBeVisible();
    const notCopied = page.getByRole("region", { name: "Not copied" });
    await expect(notCopied.getByText("Thumbnails", { exact: true })).toBeVisible();
    await shot(page, "migration");
  });
});
