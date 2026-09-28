import { test, expect, type Page } from "@playwright/test";

/**
 * The Migration page against a real `hs serve` and a real Synapse database, nothing mocked: an
 * operator points the server at Synapse on the page (no file is edited), copies, verifies, ticks
 * the checklist and cuts over, and the people Synapse served can carry on.
 *
 * Needs, besides `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN`:
 *
 * - a server named `fixture.test`, booted with the fixture's signing key, that has not migrated
 *   anything yet;
 * - `HS_REAL_MIGRATION_SOURCE`: a JSON object `{host, port, database, user, password,
 *   media_store_path}` naming a PostgreSQL database loaded with
 *   `crates/hs-compat/tests/fixtures/synapse-small/{schema,data}.sql`, and that fixture's
 *   `media_store` directory;
 * - `HS_REAL_MIGRATION_FACTS`: the path of that fixture's `facts.json`.
 *
 * `docs/compat/synapse-migration-runbook.md` ("Rehearse it") has the commands. Screenshots go to
 * `test-results/real-migration-*.png`.
 */
const server = process.env.HS_REAL_SERVER_URL;
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;
const source = process.env.HS_REAL_MIGRATION_SOURCE;
const factsPath = process.env.HS_REAL_MIGRATION_FACTS;

async function signIn(page: Page) {
  await page.goto("/");
  await page.getByRole("tab", { name: "Access token" }).click();
  await page.getByLabel("Access token").fill(adminToken!);
  await page.getByRole("button", { name: "Sign in" }).click();
  await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();
}

async function shot(page: Page, name: string) {
  await page.screenshot({ path: `test-results/real-migration-${name}.png`, fullPage: true });
}

test.describe("migration against the real server", () => {
  test.skip(
    !server || !adminToken || !source || !factsPath,
    "HS_REAL_SERVER_URL, HS_REAL_ADMIN_TOKEN, HS_REAL_MIGRATION_SOURCE and HS_REAL_MIGRATION_FACTS are needed",
  );

  test("an operator migrates a Synapse database from the page, and its users carry on", async ({
    page,
  }) => {
    test.setTimeout(180_000);
    const db = JSON.parse(source!) as {
      host: string;
      port: number;
      database: string;
      user: string;
      password: string;
      media_store_path: string;
    };
    const fs = await import("node:fs");
    const facts = JSON.parse(fs.readFileSync(factsPath!, "utf8")) as Record<string, string>;

    await signIn(page);
    await page.goto("/admin/migration");
    await expect(page.getByRole("button", { name: "Start copying" })).toBeDisabled();
    await shot(page, "1-nothing-set");

    const form = page.getByRole("form", { name: "Synapse source" });
    await form.getByLabel(/^Host/).fill(db.host);
    await form.getByLabel(/^Port/).fill(String(db.port));
    await form.getByLabel(/^Database/).fill(db.database);
    await form.getByLabel(/^User/).fill(db.user);
    await form.getByLabel(/^Password/).fill(db.password);
    await form.getByLabel(/^Media store path/).fill(db.media_store_path);
    await form.getByLabel(/^Rows per batch/).fill("2");
    await form.getByRole("button", { name: "Save source" }).click();
    await expect(page.getByText("Set, hidden")).toBeVisible();
    // The password never comes back from the server.
    const section = await (
      await fetch(`${server}/api/v1/config/migration`, {
        headers: { authorization: `Bearer ${adminToken}` },
      })
    ).json();
    expect(section.values.synapse.database.password).toEqual({ $secret: true });

    await page.getByRole("button", { name: "Start copying" }).click();
    await expect(page.getByText("Ready for cutover", { exact: true })).toBeVisible({
      timeout: 90_000,
    });
    const copied = page.getByRole("table", { name: "What has been copied" });
    await expect(copied.getByRole("row", { name: /^Accounts/ })).toContainText("4");
    await expect(copied.getByRole("row", { name: /^Rooms/ })).toContainText("2");
    await shot(page, "2-copied");

    await page.getByRole("button", { name: "Verify" }).click();
    await expect(page.getByText("Everything matches")).toBeVisible({ timeout: 60_000 });
    await shot(page, "3-verified");

    await page.getByRole("checkbox", { name: "Synapse is stopped" }).check();
    await page.getByRole("checkbox", { name: /will reach this server/ }).check();
    await page.getByRole("button", { name: "Cut over", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Cut over now" }).click();
    await expect(page.getByText(/This server is the one in service/)).toBeVisible({
      timeout: 60_000,
    });
    await expect(page.getByText(/cut over by @/).first()).toBeVisible();
    await shot(page, "4-completed");

    // alice's Synapse session carries on, without signing in again.
    const whoami = await fetch(`${server}/_matrix/client/v3/account/whoami`, {
      headers: { authorization: `Bearer ${facts.alice_token}` },
    });
    expect(whoami.status).toBe(200);
    expect((await whoami.json()).user_id).toBe("@alice:fixture.test");
    // And she is in her rooms.
    const joined = await (
      await fetch(`${server}/_matrix/client/v3/joined_rooms`, {
        headers: { authorization: `Bearer ${facts.alice_token}` },
      })
    ).json();
    expect(joined.joined_rooms).toEqual(expect.arrayContaining([facts.lobby, facts.dm]));
  });
});
