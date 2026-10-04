import { test, expect, type Page } from "@playwright/test";
import { settle } from "./settle";

/**
 * Federation, the Overview, Cluster and Statistics at scale, against a real `hs serve`, nothing
 * mocked. The suite makes its own failing destinations: it lets federation reach loopback for the
 * run (`federation.ip_range_allowlist`, which applies to the running server, put back after) and
 * invites a user on each of 55 closed local ports, so the server records 55 destinations whose
 * requests fail, more than the Federation page's page of 50. Every number the pages should show
 * is read from the same server's admin API, so nothing about its state is assumed.
 *
 * The cluster test adapts: on a single node it checks the page says there are no heartbeats or
 * drains to count; on a cluster (two replicas on one PostgreSQL, as
 * `crates/hs-cli/tests/cluster_admin.rs` starts them) it checks the heartbeat sequence, that the
 * page sees it advance between polls, and the drains released at once.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (see `real-server.spec.ts`).
 * Screenshots go to `test-results/real-scale-*.png`.
 */
const server = process.env.HS_REAL_SERVER_URL;
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;

/** How many unreachable destinations the suite makes: more than one page of 50. */
const DESTINATIONS = 55;
const FIRST_PORT = 20_001;

async function call(method: string, url: string, body?: unknown, contentType?: string) {
  const response = await fetch(url, {
    method,
    headers: {
      authorization: `Bearer ${adminToken}`,
      ...(body === undefined ? {} : { "content-type": contentType ?? "application/json" }),
      ...(method === "POST" ? { "idempotency-key": `scale-${Date.now()}-${Math.random()}` } : {}),
    },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  return { status: response.status, json: text ? JSON.parse(text) : null };
}

const api = (method: string, path: string, body?: unknown) =>
  call(
    method,
    `${server}/api/v1${path}`,
    body,
    method === "PATCH" ? "application/merge-patch+json" : undefined,
  );

/** `total` of `GET /federation/destinations` with `query`. */
async function destinationTotal(query: string): Promise<number> {
  const { status, json } = await api("GET", `/federation/destinations?include_total=true&${query}`);
  expect(status, `destinations ${query}`).toBe(200);
  return json.total;
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
  await page.screenshot({ path: `test-results/real-scale-${name}.png`, fullPage: true });
}

const count = (n: number) => n.toLocaleString("en-US");

test.describe("at scale, against the real server", () => {
  test.skip(!server || !adminToken, "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed");
  test.describe.configure({ mode: "serial" });

  let previousAllowlist: unknown = null;

  test.beforeAll(async () => {
    test.setTimeout(120_000);
    const before = await api("GET", "/config/federation");
    expect(before.status).toBe(200);
    previousAllowlist = before.json.values.ip_range_allowlist ?? null;
    const allowed = await api("PATCH", "/config/federation", {
      ip_range_allowlist: ["127.0.0.1/32"],
    });
    expect(allowed.status, JSON.stringify(allowed.json)).toBe(200);

    const client = `${server}/_matrix/client/v3`;
    const room = await call("POST", `${client}/createRoom`, { name: "scale" });
    expect(room.status, JSON.stringify(room.json)).toBe(200);
    for (let i = 0; i < DESTINATIONS; i += 1) {
      // Refused: nothing listens there. The attempt is what records the destination.
      await call("POST", `${client}/rooms/${encodeURIComponent(room.json.room_id)}/invite`, {
        user_id: `@scale:127.0.0.1:${FIRST_PORT + i}`,
      });
    }
    await expect
      .poll(() => destinationTotal("failing=true"), { timeout: 30_000 })
      .toBeGreaterThanOrEqual(DESTINATIONS);
  });

  test.afterAll(async () => {
    await api("PATCH", "/config/federation", { ip_range_allowlist: previousAllowlist });
  });

  test("Federation pages, filters and sorts through the server", async ({ page }) => {
    test.setTimeout(90_000);
    const total = await destinationTotal("");
    const failing = await destinationTotal("failing=true");
    const notFailing = await destinationTotal("failing=false");
    expect(total).toBeGreaterThan(50);
    const { json: byFailingSince } = await api(
      "GET",
      "/federation/destinations?failing=true&sort=failing_since&limit=1",
    );

    await signIn(page);
    await page.goto("/admin/federation");
    const table = page.getByRole("table", { name: "Federation destinations" });
    await expect(page.getByText(`${count(total)} servers`, { exact: true })).toBeVisible();
    await expect(table.getByRole("row")).toHaveCount(51);
    await expect(page.getByText("Failing servers first, then the rest by name.")).toBeVisible();
    await shot(page, "federation");

    await page.getByRole("button", { name: "Next" }).click();
    await expect(page).toHaveURL(/cursor=/);
    await expect(table.getByRole("row")).toHaveCount(Math.min(total - 50, 50) + 1);
    await page.getByRole("button", { name: "Previous" }).click();
    await expect(table.getByRole("row")).toHaveCount(51);

    await page.getByRole("button", { name: "Failing", exact: true }).click();
    await expect(page).toHaveURL(/show=failing/);
    await expect(page.getByText(`${count(failing)} servers failing`)).toBeVisible();
    await expect(page.getByText("Longest failing first.")).toBeVisible();
    // Longest failing first, as the server orders `sort=failing_since`.
    await expect(table.getByRole("row").nth(1)).toContainText(byFailingSince.items[0].server_name);
    await shot(page, "federation-failing");

    await page.getByRole("button", { name: "Not failing" }).click();
    await expect(page).toHaveURL(/show=not-failing/);
    if (notFailing === 0) {
      await expect(page.getByText("Every known server is failing")).toBeVisible();
    } else {
      await expect(page.getByText(`${count(notFailing)} servers not failing`)).toBeVisible();
    }

    await page.getByRole("button", { name: "Every server" }).click();
    await page
      .getByRole("columnheader", { name: /Server/ })
      .getByRole("button")
      .click();
    await expect(page).toHaveURL(/sort=server_name/);
    const { json: byName } = await api("GET", "/federation/destinations?sort=server_name&limit=1");
    await expect(table.getByRole("row").nth(1)).toContainText(byName.items[0].server_name);
    await page
      .getByRole("columnheader", { name: /Server/ })
      .getByRole("button")
      .click();
    await expect(page).toHaveURL(/sort=-server_name/);
    const { json: byNameDesc } = await api(
      "GET",
      "/federation/destinations?sort=-server_name&limit=1",
    );
    await expect(table.getByRole("row").nth(1)).toContainText(byNameDesc.items[0].server_name);
  });

  test("the Overview counts failing servers from the server's field", async ({ page }) => {
    test.setTimeout(120_000);
    const failing = await destinationTotal("failing=true");
    // The Overview's counts are a snapshot recounted at most once a minute; wait until it has
    // counted the destinations this suite made.
    await expect
      .poll(
        async () =>
          (await api("GET", "/statistics/overview")).json.federation_destinations_failing_count,
        { timeout: 90_000, intervals: [2_000] },
      )
      .toBe(failing);
    const notFailing = await destinationTotal("failing=false");

    await signIn(page);
    const strip = page.getByRole("region", { name: "Federation" });
    await expect(
      strip.getByRole("button", { name: new RegExp(`^${count(failing)} Failing$`) }),
    ).toBeVisible();
    await expect(
      strip.getByRole("button", { name: new RegExp(`^${count(notFailing)} Not failing$`) }),
    ).toBeVisible();
    // The suite's failures are minutes old, so none is flagged as failing for over an hour.
    await expect(page.getByText(/failing for over an hour/)).toHaveCount(0);
    await shot(page, "overview");

    await strip
      .getByRole("button", { name: /Failing$/ })
      .first()
      .click();
    await expect(page).toHaveURL(/\/admin\/federation\?show=failing$/);
    await expect(page.getByText(`${count(failing)} servers failing`)).toBeVisible();
  });

  test("Cluster and Statistics show the cluster's heartbeat and drains", async ({ page }) => {
    test.setTimeout(90_000);
    const { json: cluster } = await api("GET", "/cluster");
    await signIn(page);
    await page.goto("/admin/cluster");
    await expect(page.getByRole("heading", { name: "Cluster", level: 1 })).toBeVisible();

    if (cluster.mode === "single-node") {
      expect(cluster.heartbeat_seq).toBeUndefined();
      await expect(page.getByText("A single node sends no heartbeats")).toBeVisible();
      await expect(page.getByText("Nothing to drain on a single node")).toBeVisible();
      await shot(page, "cluster-single-node");
      await page.goto("/admin/statistics");
      await expect(page.getByText(/This server runs as a single node: one replica/)).toBeVisible();
      await shot(page, "statistics-single-node");
      return;
    }

    expect(cluster.heartbeat_seq).toBeGreaterThan(0);
    expect(cluster.drain_released_at_once_count).toBeGreaterThanOrEqual(0);
    const replicas = page.getByRole("table", { name: "Replicas" });
    const me = replicas.getByRole("row").filter({ hasText: "This replica" });
    await expect(me).toContainText(/seq [\d,]+/);
    // The page's next poll (15 s) reads a higher number: heartbeats are still arriving.
    await expect(me).toContainText(/\+[\d,]+ since the last poll/, { timeout: 40_000 });
    await expect(page.getByText("This replica, still arriving")).toBeVisible();
    await expect(page.getByText("Since this replica started")).toBeVisible();
    await shot(page, "cluster");

    await page.goto("/admin/statistics");
    const section = page.getByRole("region", { name: "Cluster" });
    await expect(section.getByText("Heartbeat sequence")).toBeVisible();
    const { json: now } = await api("GET", "/cluster");
    await expect(section).toContainText(count(now.drain_released_at_once_count));
    await expect(section).toContainText(count(now.replica_count));
    await shot(page, "statistics-cluster");
  });
});
