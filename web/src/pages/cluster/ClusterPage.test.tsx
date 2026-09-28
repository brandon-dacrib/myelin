import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { drainReplica, getReplica, listShards, setDrainDuration } from "@/mocks/data/cluster";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { ClusterPage } from "./ClusterPage";
import { validateClusterSearch } from "./cluster-search";

const ROUTES = [
  { path: "/cluster", component: ClusterPage, validateSearch: validateClusterSearch },
];
const KNOWN = ["/tasks/$taskId", "/configuration/$section"];

function open(path = "/cluster") {
  return renderRoutes(ROUTES, path, KNOWN);
}

/** The Replicas table's row for `id`. */
async function replicaRow(id: string) {
  const table = await screen.findByRole("table", { name: "Replicas" });
  const cell = await within(table).findByText(id);
  return within(cell.closest("tr")!);
}

function owned(id: string): number {
  return listShards(null).filter((s) => s.owner === id).length;
}

const SINGLE_NODE_REPLICA = {
  id: "hs-single",
  role: "single-node",
  status: "active",
  shard_count: 3,
  epoch: 1,
  this_replica: true,
  mesh_addr: null,
  version: "0.1.0",
  zone: null,
  last_heartbeat_at: null,
  drain_requested_at: null,
  drain_requested_by: null,
  drain_task_id: null,
};

function singleNode() {
  server.use(
    http.get("/api/v1/cluster", () =>
      HttpResponse.json({ mode: "single-node", replica_count: 1, shard_count: 3, epoch: 1 }),
    ),
    http.get("/api/v1/cluster/replicas", () =>
      HttpResponse.json({ items: [SINGLE_NODE_REPLICA], next_cursor: null, prev_cursor: null }),
    ),
    http.get("/api/v1/cluster/shards", () =>
      HttpResponse.json({
        items: [
          { kind: "room", id: "room/0", owner: "hs-single", state: "owned", epoch: 1 },
          { kind: "room", id: "room/1", owner: "hs-single", state: "owned", epoch: 1 },
          { kind: "global", id: "global/0", owner: "hs-single", state: "owned", epoch: 1 },
        ],
        next_cursor: null,
        prev_cursor: null,
      }),
    ),
  );
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("Cluster", () => {
  it("summarises the cluster and lists each replica with its shards", async () => {
    open();
    const hs0 = await replicaRow("hs-0");
    expect(hs0.getByText("This replica")).toBeInTheDocument();
    expect(hs0.getByText("Active")).toBeInTheDocument();
    expect(hs0.getByText(String(owned("hs-0")))).toBeInTheDocument();
    expect(hs0.getByText("eu-west-1a")).toBeInTheDocument();
    expect(hs0.getByText("10.0.1.10:7600")).toBeInTheDocument();
    expect(hs0.getByText("just now")).toBeInTheDocument();
    const hs1 = await replicaRow("hs-1");
    expect(hs1.queryByText("This replica")).not.toBeInTheDocument();
    expect(hs1.getByRole("button", { name: "Drain hs-1" })).toBeEnabled();

    expect(screen.getByText("Cluster", { selector: "dd" })).toBeInTheDocument();
    expect(screen.getByText("3 active")).toBeInTheDocument();
    const total = listShards(null).length;
    expect(await screen.findByText(`${total} of ${total}`)).toBeInTheDocument();
    expect(screen.getByText("Every shard has an owner")).toBeInTheDocument();

    // The map: a legend that counts, and each kind as one image named in words.
    const owners = screen.getByRole("list", { name: "Owners" });
    expect(within(owners).getByText("(this replica)")).toBeInTheDocument();
    expect(within(owners).getByText(`${owned("hs-1")} shards`)).toBeInTheDocument();
    expect(screen.getByRole("img", { name: /^64 room shards: / })).toBeInTheDocument();
    expect(screen.getByRole("img", { name: /^1 global shard: hs-\d owns 1$/ })).toBeInTheDocument();
  });

  it("drains a replica after saying what that does, and follows it until it is drained", async () => {
    setDrainDuration(1_200);
    // Shard reads made once the drain has finished on the server.
    let settledShardReads = 0;
    server.use(
      http.get("/api/v1/cluster/shards", () => {
        if (getReplica("hs-1")?.status === "drained") settledShardReads += 1;
        return undefined;
      }),
    );
    const user = userEvent.setup();
    const before = { hs1: owned("hs-1"), hs0: owned("hs-0") };
    open();
    const hs1 = await replicaRow("hs-1");
    await user.click(hs1.getByRole("button", { name: "Drain hs-1" }));

    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByRole("heading", { name: "Drain hs-1?" })).toBeInTheDocument();
    expect(dialog.getByText(/other active replicas/)).toBeInTheDocument();
    expect(dialog.getByText(/they move to hs-0 and hs-2/)).toBeInTheDocument();
    expect(dialog.getByText(/survives a restart/)).toBeInTheDocument();
    expect(dialog.getByText(/Undrain gives it its share/)).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Drain replica" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(await hs1.findByText("Draining")).toBeInTheDocument();
    expect(hs1.getByRole("progressbar")).toBeInTheDocument();
    expect(hs1.getByText(/@admin:example\.org/)).toBeInTheDocument();
    expect(hs1.getByRole("link", { name: "Drain task for hs-1" })).toHaveAttribute(
      "href",
      expect.stringMatching(/^\/tasks\/task_drain_/),
    );
    // Undrain is offered from the moment it is asked to drain.
    expect(hs1.getByRole("button", { name: "Undrain hs-1" })).toBeEnabled();

    await waitFor(() => expect(hs1.getByText("Drained")).toBeInTheDocument(), { timeout: 8_000 });
    expect(hs1.getByText("0")).toBeInTheDocument();
    expect(hs1.queryByRole("progressbar")).not.toBeInTheDocument();
    const hs0 = await replicaRow("hs-0");
    await waitFor(() =>
      expect(hs0.getByText(String(before.hs0 + Math.ceil(before.hs1 / 2)))).toBeInTheDocument(),
    );
    // The shards are read again as soon as the drain settles, not at the next slow poll (15s
    // later), so the summary does not go on counting a shard that was between owners.
    await waitFor(() => expect(settledShardReads).toBeGreaterThan(0), { timeout: 3_000 });
    await waitFor(() => expect(screen.getByText("Every shard has an owner")).toBeInTheDocument());
  }, 15_000);

  it("undrains a drained replica, which takes its shards back", async () => {
    setDrainDuration(0);
    const before = owned("hs-2");
    drainReplica("hs-2");
    const user = userEvent.setup();
    open();
    const hs2 = await replicaRow("hs-2");
    expect(hs2.getByText("Drained")).toBeInTheDocument();
    expect(hs2.getByText("0")).toBeInTheDocument();
    await user.click(hs2.getByRole("button", { name: "Undrain hs-2" }));

    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByText(/takes back its share/)).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Undrain replica" }));

    expect(await hs2.findByText("Active")).toBeInTheDocument();
    await waitFor(() => expect(hs2.getByText(String(before))).toBeInTheDocument());
    expect(hs2.getByRole("button", { name: "Drain hs-2" })).toBeEnabled();
    expect(hs2.queryByRole("link", { name: /Drain task/ })).not.toBeInTheDocument();
  });

  it("shows the server's own reason when it refuses a drain", async () => {
    server.use(
      http.post("/api/v1/cluster/replicas/:id/drain", () =>
        HttpResponse.json(
          {
            type: "urn:hs:problem:conflict",
            title: "Conflict",
            status: 409,
            detail: "no other replica is active to take hs-1's shards",
          },
          { status: 409 },
        ),
      ),
    );
    const user = userEvent.setup();
    open();
    const hs1 = await replicaRow("hs-1");
    await user.click(hs1.getByRole("button", { name: "Drain hs-1" }));
    const dialog = within(await screen.findByRole("dialog"));
    await user.click(dialog.getByRole("button", { name: "Drain replica" }));
    expect(await dialog.findByRole("alert")).toHaveTextContent(
      "Couldn't drain it. no other replica is active to take hs-1's shards",
    );
    expect(hs1.getByText("Active")).toBeInTheDocument();
  });

  it("says what a refused drain means from its reason, not its detail", async () => {
    server.use(
      http.post("/api/v1/cluster/replicas/:id/drain", () =>
        HttpResponse.json(
          {
            type: "urn:hs:problem:conflict",
            title: "Conflict",
            status: 409,
            detail: "prose that the page must not need to parse",
            reason: "no_other_active_replica",
          },
          { status: 409 },
        ),
      ),
    );
    const user = userEvent.setup();
    open();
    const hs1 = await replicaRow("hs-1");
    await user.click(hs1.getByRole("button", { name: "Drain hs-1" }));
    const dialog = within(await screen.findByRole("dialog"));
    await user.click(dialog.getByRole("button", { name: "Drain replica" }));
    const alert = await dialog.findByRole("alert");
    expect(alert).toHaveTextContent(
      "Couldn't drain it. No other replica is active to take its shards.",
    );
    expect(alert).not.toHaveTextContent("prose");
  });

  it("offers no drain to a single node, and says why", async () => {
    singleNode();
    open();
    const row = await replicaRow("hs-single");
    expect(row.getByText("This replica")).toBeInTheDocument();
    expect(row.getByText("Single node")).toBeInTheDocument();
    expect(row.getByText("Active")).toBeInTheDocument();
    const drain = row.getByRole("button", { name: "Drain hs-single" });
    expect(drain).toBeDisabled();
    expect(drain).toHaveAccessibleDescription(/Nothing to drain to/);
    expect(screen.getByRole("note")).toHaveTextContent("there is nothing to drain it to");
    expect(screen.getByText("Single node", { selector: "dd" })).toBeInTheDocument();
    expect(await screen.findByText("3 of 3")).toBeInTheDocument();
    expect(screen.getByText(/single node: one replica that owns every shard/)).toBeInTheDocument();
  });

  it("lists a stopped drained replica as not running, and undrains it by its encoded id", async () => {
    const stopped = {
      id: "127.0.0.1:18450",
      role: "replica",
      status: "drained",
      shard_count: 0,
      epoch: 0,
      this_replica: false,
      mesh_addr: null,
      version: "0.1.0",
      zone: null,
      last_heartbeat_at: null,
      drain_requested_at: new Date(Date.now() - 3_600_000).toISOString(),
      drain_requested_by: "@admin:example.org",
      drain_task_id: null,
    };
    let undrainPath: string | undefined;
    server.use(
      http.get("/api/v1/cluster/replicas", () =>
        HttpResponse.json({
          items: [
            {
              ...SINGLE_NODE_REPLICA,
              id: "127.0.0.1:18449",
              role: "replica",
              mesh_addr: "127.0.0.1:18449",
              last_heartbeat_at: new Date().toISOString(),
            },
            stopped,
          ],
          next_cursor: null,
          prev_cursor: null,
        }),
      ),
      http.post("/api/v1/cluster/replicas/:id/undrain", ({ request }) => {
        undrainPath = new URL(request.url).pathname;
        return HttpResponse.json({ ...stopped, status: "active", drain_requested_at: null });
      }),
    );
    const user = userEvent.setup();
    open();
    const row = await replicaRow("127.0.0.1:18450");
    expect(row.getByText("Drained")).toBeInTheDocument();
    expect(row.getAllByText("Not running").length).toBeGreaterThan(0);
    expect(row.getByText(/stays listed, and drained, until you undrain it/)).toBeInTheDocument();
    await user.click(row.getByRole("button", { name: "Undrain 127.0.0.1:18450" }));
    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByText(/the next time it starts/)).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Undrain replica" }));
    await waitFor(() =>
      expect(undrainPath).toBe("/api/v1/cluster/replicas/127.0.0.1%3A18450/undrain"),
    );
  });

  it("does not offer a drain when no other replica could take the shards", async () => {
    setDrainDuration(0);
    drainReplica("hs-1");
    drainReplica("hs-2");
    open();
    const hs0 = await replicaRow("hs-0");
    expect(hs0.getByRole("button", { name: "Drain hs-0" })).toBeDisabled();
    expect(hs0.getByText("No other replica is active to take its shards.")).toBeInTheDocument();
  });

  it("filters shards by kind, on the map and in the table", async () => {
    const user = userEvent.setup();
    let asked: URLSearchParams | undefined;
    open();
    expect(await screen.findByRole("heading", { name: /^Rooms/ })).toBeInTheDocument();

    // Opened from the keyboard, the way jsdom can open a Radix select.
    screen.getByRole("combobox", { name: "Kind" }).focus();
    await user.keyboard("{Enter}");
    await user.click(await screen.findByRole("option", { name: "Users" }));
    expect(await screen.findByRole("heading", { name: /^Users/ })).toBeInTheDocument();
    expect(screen.queryByRole("heading", { name: /^Rooms/ })).not.toBeInTheDocument();
    expect(screen.getByRole("img", { name: /^32 user shards/ })).toBeInTheDocument();

    server.events.on("request:start", ({ request }) => {
      const url = new URL(request.url);
      if (url.pathname === "/api/v1/cluster/shards" && url.searchParams.has("kind")) {
        asked = url.searchParams;
      }
    });
    await user.click(screen.getByRole("button", { name: "Table" }));
    const table = within(await screen.findByRole("table", { name: "Shards" }));
    expect(await table.findByText("user/0")).toBeInTheDocument();
    expect(table.queryByText("room/0")).not.toBeInTheDocument();
    expect(asked?.get("kind")).toBe("user");
    server.events.removeAllListeners();
  });

  it("says plainly when the server has no cluster source wired", async () => {
    server.use(
      http.get("/api/v1/cluster/replicas", () =>
        HttpResponse.json(
          { type: "urn:hs:problem:unavailable", title: "Unavailable", status: 503 },
          { status: 503 },
        ),
      ),
    );
    open();
    expect(
      await screen.findByText("Replicas isn't connected to a data source on this server yet"),
    ).toBeInTheDocument();
    expect(screen.queryByRole("table", { name: "Replicas" })).not.toBeInTheDocument();
  });

  it("does not show the cluster without admin:read", async () => {
    await signIn(["moderation:read"]);
    open();
    expect(await screen.findByText(/This needs the/)).toHaveTextContent("admin:read");
  });
});
