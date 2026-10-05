import { test, expect, type Page } from "@playwright/test";
import { settle } from "./settle";

/**
 * The Cluster page against a real `hs serve`, nothing mocked, whichever way the server runs:
 *
 * - A single node: one replica, marked as the one answering and as a single node, active and
 *   owning every shard of the layout; drain is not offered, with the reason on the page, because
 *   a single node has nothing to drain to.
 * - A cluster (several replicas on one PostgreSQL): every replica listed with its status and
 *   shard count as the server reports them, one of them marked as the one answering, every shard
 *   owned by a listed replica, and drain offered for an active replica while another active one
 *   could take its shards (nothing is drained: the server may be shared).
 *
 * Either way the shard table lists the room shards with their owners, and each replica's Epoch
 * (its generation, the wall clock in milliseconds when it started) is a short time with the raw
 * number in the tooltip, and the Replicas table fits the 1280 px viewport.
 *
 * Needs `HS_REAL_SERVER_URL` and `HS_REAL_ADMIN_TOKEN` (playwright.real.config.ts). The page is
 * checked against what the same server answers through the admin API, so neither the layout's
 * size nor the number of replicas is assumed. Screenshots go to
 * `test-results/real-cluster-*.png`.
 */
const server = process.env.HS_REAL_SERVER_URL;
const adminToken = process.env.HS_REAL_ADMIN_TOKEN;

interface Replica {
  id: string;
  role: string;
  status: string;
  shard_count: number;
  epoch: number;
  this_replica: boolean;
}

/** The status pill's words, as `src/lib/cluster.ts` has them. */
const STATUS_LABELS: Record<string, string> = {
  joining: "Joining",
  active: "Active",
  draining: "Draining",
  drained: "Drained",
  unreachable: "Unreachable",
};

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
  await settle(page);
  await page.screenshot({ path: `test-results/real-cluster-${name}.png`, fullPage: true });
}

test.describe("cluster against the real server", () => {
  test.skip(!server || !adminToken, "HS_REAL_SERVER_URL and HS_REAL_ADMIN_TOKEN are needed");

  test("every replica and shard as the server reports them, single node or cluster", async ({
    page,
  }) => {
    test.setTimeout(90_000);
    await page.goto("/");
    await page.getByRole("tab", { name: "Access token" }).click();
    await page.getByLabel("Access token").fill(adminToken!);
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page.getByRole("tab", { name: "Access token" })).toBeHidden();

    // What the server says about itself, through the operations the page reads.
    const replicas = await fetchAll<Replica>("/cluster/replicas");
    expect(replicas.length).toBeGreaterThan(0);
    const singleNode = replicas.length === 1 && replicas[0].role === "single-node";
    const ids = new Set(replicas.map((r) => r.id));
    const shards = await fetchAll<{ id: string; owner: string | null }>("/cluster/shards");
    expect(shards.length).toBeGreaterThan(0);
    expect(shards.every((s) => s.owner && ids.has(s.owner))).toBe(true);
    const total = shards.length.toLocaleString("en-US");
    const answering = replicas.filter((r) => r.this_replica);
    expect(answering).toHaveLength(1);

    await page.goto("/admin/cluster");
    await expect(page.getByRole("heading", { name: "Cluster", level: 1 })).toBeVisible();
    await expect(
      page.getByText(
        singleNode
          ? "This server runs as a single node"
          : "The replicas serving this server share its work as shards",
      ),
    ).toBeVisible();
    await expect(page.getByText(`${total} of ${total}`)).toBeVisible();
    await expect(page.getByText("Every shard has an owner")).toBeVisible();

    const table = page.getByRole("table", { name: "Replicas" });
    const rows = table.getByRole("row");
    await expect(rows).toHaveCount(replicas.length + 1);
    // Re-read: shard counts move while a cluster settles, and the page polls.
    const settled = await fetchAll<Replica>("/cluster/replicas");
    for (const replica of settled) {
      const row = rows.filter({ hasText: replica.id });
      if (replica.this_replica) await expect(row).toContainText("This replica");
      else await expect(row).not.toContainText("This replica");
      await expect(row).toContainText(STATUS_LABELS[replica.status] ?? replica.status);
      if (singleNode) await expect(row).toContainText("Single node");
      await expect(row.getByRole("cell").nth(2)).toHaveText(
        replica.shard_count.toLocaleString("en-US"),
      );
      // The Epoch column: a short start time, the number in the tooltip.
      if (replica.epoch) {
        const epoch = row.getByTitle(new RegExp(`^Generation ${replica.epoch}\\b`));
        await expect(epoch).toBeVisible();
        await expect(epoch).not.toHaveText(/\d{5,}/);
        const box = await epoch.boundingBox();
        const cell = await epoch.locator("xpath=ancestor::td[1]").boundingBox();
        expect(box && cell && box.x + box.width <= cell.x + cell.width + 1).toBe(true);
      }
    }
    // The table fits the viewport: nothing scrolls sideways at 1280 px.
    const overflow = await table.evaluate((el) => {
      const scroller = el.parentElement!;
      return { scroll: scroller.scrollWidth, client: scroller.clientWidth };
    });
    expect(overflow.scroll).toBeLessThanOrEqual(overflow.client);
    expect(await page.evaluate(() => document.documentElement.scrollWidth)).toBeLessThanOrEqual(
      page.viewportSize()!.width,
    );

    if (singleNode) {
      const [replica] = replicas;
      const row = rows.filter({ hasText: replica.id });
      await expect(row.getByRole("cell").nth(2)).toHaveText(total);
      // No drain to offer, and the page says why rather than leaving a button that can only fail.
      await expect(row.getByRole("button", { name: `Drain ${replica.id}` })).toBeDisabled();
      await expect(row).toContainText("Nothing to drain to");
      await expect(page.getByRole("note")).toContainText("there is nothing to drain it to");
    } else {
      await expect(page.getByText("This server runs as a single node")).toHaveCount(0);
      const active = settled.filter((r) => r.status === "active");
      for (const replica of active) {
        const button = rows
          .filter({ hasText: replica.id })
          .getByRole("button", { name: `Drain ${replica.id}` });
        // A drain needs another active replica to take the shards.
        if (active.length > 1) await expect(button).toBeEnabled();
        else await expect(button).toBeDisabled();
      }
    }
    // The map: every owner is listed, nothing is unowned.
    const owners = page.getByRole("list", { name: "Owners" });
    for (const owner of new Set(shards.map((s) => s.owner!))) {
      await expect(owners).toContainText(owner);
    }
    await expect(page.getByRole("img", { name: /unowned/ })).toHaveCount(0);
    await shot(page, singleNode ? "single-node" : "replicas");

    // The room shards, one by one, each owned by a listed replica.
    await page.getByRole("combobox", { name: "Kind" }).click();
    await page.getByRole("option", { name: "Rooms" }).click();
    await page.getByRole("button", { name: "Table" }).click();
    const shardTable = page.getByRole("table", { name: "Shards" });
    await expect(shardTable).toContainText("room/0");
    const shardRows = shardTable.getByRole("row");
    const count = await shardRows.count();
    expect(count).toBeGreaterThan(1);
    const owner = new RegExp(
      [...ids].map((id) => id.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|"),
    );
    for (let i = 1; i < count; i += 1) {
      await expect(shardRows.nth(i)).toContainText("room/");
      await expect(shardRows.nth(i)).toContainText(owner);
      await expect(shardRows.nth(i)).toContainText("Owned");
    }
    await expect(shardTable).not.toContainText("unowned");
    await shot(page, "room-shards");
  });
});
