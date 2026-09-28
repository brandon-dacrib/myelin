import { test, expect, type Page } from "@playwright/test";

/**
 * The Cluster page against a real single-node `hs serve`, nothing mocked: one replica, marked as
 * the one answering and as a single node, active and owning every shard of the layout; the shard
 * table lists the room shards with it as their owner; and drain is not offered, with the reason
 * on the page, because a single node has nothing to drain to.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts), against a
 * server started without cluster mode (`hs serve --data-dir ... --server-name ...`). The page is
 * checked against what the same server answers through the admin API, so the layout's size is
 * not assumed. Screenshots go to `test-results/real-cluster-*.png`.
 */
const server = process.env.HS_REAL_SERVER_URL;
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;

interface PageOf<T> {
  items: T[];
  next_cursor: string | null;
}

/** Every item of a paged admin API list, straight from the server. */
async function fetchAll<T>(path: string, query = ""): Promise<T[]> {
  const all: T[] = [];
  let cursor: string | null = null;
  for (let i = 0; i < 20; i += 1) {
    const params = new URLSearchParams(query);
    params.set("limit", "500");
    if (cursor) params.set("cursor", cursor);
    const response = await fetch(`${server}/api/v1${path}?${params}`, {
      headers: { authorization: `Bearer ${adminToken}` },
    });
    expect(response.status, `GET ${path}`).toBe(200);
    const page = (await response.json()) as PageOf<T>;
    all.push(...page.items);
    cursor = page.next_cursor;
    if (!cursor) break;
  }
  return all;
}

async function shot(page: Page, name: string) {
  await page.waitForLoadState("networkidle");
  await page.screenshot({ path: `test-results/real-cluster-${name}.png`, fullPage: true });
}

test.describe("cluster against the real server", () => {
  test.skip(!server || !adminToken, "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed");

  test("a single node: one replica owns every shard, and drain is not offered", async ({
    page,
  }) => {
    test.setTimeout(90_000);
    await page.goto("/");
    await page.getByRole("tab", { name: "Access token" }).click();
    await page.getByLabel("Access token").fill(adminToken!);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();

    // What the server says about itself, through the operations the page reads.
    const replicas = await fetchAll<{ id: string; role: string; shard_count: number }>(
      "/cluster/replicas",
    );
    expect(replicas).toHaveLength(1);
    const [replica] = replicas;
    expect(replica.role).toBe("single-node");
    const shards = await fetchAll<{ id: string; owner: string | null }>("/cluster/shards");
    expect(shards.length).toBeGreaterThan(0);
    expect(shards.every((s) => s.owner === replica.id)).toBe(true);
    expect(replica.shard_count).toBe(shards.length);
    const total = shards.length.toLocaleString("en-US");

    await page.goto("/admin/cluster");
    await expect(page.getByRole("heading", { name: "Cluster", level: 1 })).toBeVisible();
    await expect(page.getByText("This server runs as a single node")).toBeVisible();
    await expect(page.getByText(`${total} of ${total}`)).toBeVisible();
    await expect(page.getByText("Every shard has an owner")).toBeVisible();

    const rows = page.getByRole("table", { name: "Replicas" }).getByRole("row");
    await expect(rows).toHaveCount(2);
    const row = rows.filter({ hasText: replica.id });
    await expect(row).toContainText("This replica");
    await expect(row).toContainText("Single node");
    await expect(row).toContainText("Active");
    await expect(row.getByRole("cell").nth(2)).toHaveText(total);

    // No drain to offer, and the page says why rather than leaving a button that can only fail.
    await expect(row.getByRole("button", { name: `Drain ${replica.id}` })).toBeDisabled();
    await expect(row).toContainText("Nothing to drain to");
    await expect(page.getByRole("note")).toContainText("there is nothing to drain it to");
    // The map: every shard is this replica's.
    await expect(page.getByRole("list", { name: "Owners" })).toContainText(replica.id);
    await expect(page.getByRole("img", { name: /unowned/ })).toHaveCount(0);
    await shot(page, "single-node");

    // The room shards, one by one, each owned by the one replica.
    await page.getByRole("combobox", { name: "Kind" }).click();
    await page.getByRole("option", { name: "Rooms" }).click();
    await page.getByRole("button", { name: "Table" }).click();
    const table = page.getByRole("table", { name: "Shards" });
    await expect(table).toContainText("room/0");
    const shardRows = table.getByRole("row");
    const count = await shardRows.count();
    expect(count).toBeGreaterThan(1);
    for (let i = 1; i < count; i += 1) {
      await expect(shardRows.nth(i)).toContainText("room/");
      await expect(shardRows.nth(i)).toContainText(replica.id);
      await expect(shardRows.nth(i)).toContainText("Owned");
    }
    await expect(table).not.toContainText("unowned");
    await shot(page, "room-shards");
  });
});
