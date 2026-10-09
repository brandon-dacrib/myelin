import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  RouterProvider,
  type AnyRouter,
} from "@tanstack/react-router";
import { http, HttpResponse } from "msw";
import { DashboardPage } from "./DashboardPage";
import { server } from "@/mocks/node";
import { signIn, signOut } from "@/lib/auth";

function renderDashboard() {
  const rootRoute = createRootRoute();
  const router = createRouter({
    routeTree: rootRoute.addChildren([
      createRoute({ getParentRoute: () => rootRoute, path: "/", component: DashboardPage }),
    ]),
    history: createMemoryHistory({ initialEntries: ["/"] }),
  }) as unknown as AnyRouter;
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
}

function notImplemented() {
  return HttpResponse.json(
    { type: "urn:hs:problem:not-implemented", title: "Not implemented", status: 501 },
    { status: 501, headers: { "Content-Type": "application/problem+json" } },
  );
}

async function tile(label: string) {
  const name = await screen.findByText(label);
  return within(name.parentElement as HTMLElement);
}

beforeEach(async () => {
  await signIn();
});

afterEach(() => {
  server.resetHandlers();
  signOut();
});

/**
 * What the real server answers today (`crates/hs-cli/src/overview.rs`): counts of accounts and
 * rooms, no media, federation or report counts, and `501` for bridges and federation.
 */
function serveLikeTheRealServer() {
  server.use(
    http.get("/api/v1/statistics/overview", () =>
      HttpResponse.json({
        users_count: 1,
        rooms_count: 0,
        daily_active_users: 1,
        monthly_active_users: 1,
      }),
    ),
    http.get("/api/v1/cluster", () => HttpResponse.json({ mode: "single-node", replica_count: 1 })),
    http.get("/api/v1/server", () =>
      HttpResponse.json({ name: "localhost", version: "0.0.1", uptime_ms: 5 * 60_000 }),
    ),
    http.get("/api/v1/appservices", notImplemented),
    http.get("/api/v1/federation/destinations", notImplemented),
    http.get("/api/v1/tasks", () => HttpResponse.json(emptyPage)),
  );
}

const emptyPage = { items: [], next_cursor: null, prev_cursor: null };

describe("Overview", () => {
  it("shows a new server's numbers, a real zero included", async () => {
    serveLikeTheRealServer();
    renderDashboard();

    expect((await tile("Users")).getByText("1")).toBeInTheDocument();
    expect((await tile("Rooms")).getByText("0")).toBeInTheDocument();
  });

  it("says what the server is in one line: name, version, mode and uptime", async () => {
    serveLikeTheRealServer();
    renderDashboard();
    const line = await screen.findByTestId("server-line");
    expect(line).toHaveTextContent("localhost");
    expect(line).toHaveTextContent("version 0.0.1");
    expect(line).toHaveTextContent("one process, which serves everything");
    expect(line).toHaveTextContent("up 5m");
    // Those three are no longer tiles.
    expect(screen.queryByText("Mode")).not.toBeInTheDocument();
    expect(screen.queryByText("Uptime")).not.toBeInTheDocument();
  });

  it("names the cluster in the line, with a link to it", async () => {
    server.use(
      http.get("/api/v1/cluster", () => HttpResponse.json({ mode: "cluster", replica_count: 3 })),
    );
    renderDashboard();
    const line = await screen.findByTestId("server-line");
    expect(line).toHaveTextContent("a cluster of 3 replicas");
    expect(within(line).getByRole("link", { name: "see Cluster" })).toBeInTheDocument();
  });

  it("offers the first steps on a server that has only its administrator", async () => {
    serveLikeTheRealServer();
    server.use(http.get("/api/v1/bridge-offerings", () => HttpResponse.json(emptyPage)));
    renderDashboard();
    const section = within(await screen.findByRole("region", { name: "Get started" }));
    expect(section.getByRole("button", { name: /Add people/ })).toBeInTheDocument();
    expect(
      section.getByRole("button", { name: /Let people sign up themselves/ }),
    ).toBeInTheDocument();
    expect(section.getByRole("button", { name: /Offer a bridge/ })).toBeInTheDocument();
    expect(section.getByRole("button", { name: /Move here from Synapse/ })).toBeInTheDocument();
  });

  it("drops the first steps once the server has people", async () => {
    // The mock's defaults: hundreds of users and two offerings.
    renderDashboard();
    await screen.findByText("Attention");
    await screen.findByTestId("server-line");
    expect(screen.queryByRole("region", { name: "Get started" })).not.toBeInTheDocument();
  });

  it("does not raise the server's own bridge manager as a bridge in trouble", async () => {
    server.use(
      http.get("/api/v1/statistics/overview", () =>
        HttpResponse.json({ users_count: 3, rooms_count: 2, pending_reports_count: 0 }),
      ),
      http.get("/api/v1/appservices", () =>
        HttpResponse.json({
          items: [{ id: "myelin-bridges", sender_localpart: "bridges", health: "unknown" }],
          next_cursor: null,
          prev_cursor: null,
        }),
      ),
      http.get("/api/v1/federation/destinations", () => HttpResponse.json(emptyPage)),
      http.get("/api/v1/tasks", () => HttpResponse.json(emptyPage)),
    );
    renderDashboard();
    expect(await screen.findByText("Nothing needs your attention.")).toBeInTheDocument();
    expect(screen.queryByText(/bridge manager is unknown/)).not.toBeInTheDocument();
    // Nor is it listed as one of the bridges: there are none, and the strip says what to do.
    const strip = within(screen.getByRole("region", { name: "Bridges" }));
    expect(strip.getByText(/No bridges yet/)).toBeInTheDocument();
    expect(strip.getByRole("link", { name: "Offer one" })).toBeInTheDocument();
  });

  it("does not give an all-clear about things it could not check", async () => {
    serveLikeTheRealServer();
    renderDashboard();

    expect(
      await screen.findByText(
        /as far as this server can tell\. It can.t check bridges, federation or reports yet\./,
      ),
    ).toBeInTheDocument();
    expect(screen.queryByText("Nothing needs your attention.")).not.toBeInTheDocument();
  });

  it("gives the plain all-clear when every source answered", async () => {
    // The mock's defaults answer everything, with one failing destination and two reports --
    // so clear those, leaving every source answering and nothing wrong.
    server.use(
      http.get("/api/v1/statistics/overview", () =>
        HttpResponse.json({ users_count: 3, rooms_count: 2, pending_reports_count: 0 }),
      ),
      http.get("/api/v1/appservices", () =>
        HttpResponse.json({ items: [], next_cursor: null, prev_cursor: null }),
      ),
      http.get("/api/v1/federation/destinations", () =>
        HttpResponse.json({ items: [], next_cursor: null, prev_cursor: null }),
      ),
      http.get("/api/v1/tasks", () => HttpResponse.json(emptyPage)),
    );
    renderDashboard();

    expect(await screen.findByText("Nothing needs your attention.")).toBeInTheDocument();
    expect(screen.queryByText(/can.t check/)).not.toBeInTheDocument();
  });

  it("counts failing destinations from the server's field, not from a page", async () => {
    server.use(
      http.get("/api/v1/statistics/overview", () =>
        HttpResponse.json({
          users_count: 3,
          rooms_count: 2,
          pending_reports_count: 0,
          federation_destinations_failing_count: 7,
        }),
      ),
      http.get("/api/v1/federation/destinations", ({ request }) => {
        const q = new URL(request.url).searchParams;
        // The failing list, longest failing first: a page of four, all down for hours, with
        // more pages behind it.
        if (q.get("failing") === "true") {
          expect(q.get("sort")).toBe("failing_since");
          return HttpResponse.json({
            items: ["a", "b", "c", "d"].map((n) => ({
              server_name: `${n}.down.example`,
              failing_since: new Date(Date.now() - 2 * 3_600_000).toISOString(),
              last_successful_at: null,
              retry_last_at: null,
              retry_interval_ms: 60_000,
              pending_pdu_count: 1,
              pending_edu_count: 0,
            })),
            next_cursor: "more",
            prev_cursor: null,
            total: 7,
          });
        }
        expect(q.get("failing")).toBe("false");
        expect(q.get("limit")).toBe("1");
        return HttpResponse.json({ items: [], next_cursor: null, prev_cursor: null, total: 58 });
      }),
    );
    renderDashboard();

    const failing = await tile("Failing");
    expect(failing.getByText("7")).toBeInTheDocument();
    expect((await tile("Not failing")).getByText("58")).toBeInTheDocument();
    expect(screen.getByText(/counted by the server/)).toBeInTheDocument();
    // Four hour-old failures are one row, not four, and the full page says "at least".
    expect(
      screen.getByText("At least 4 servers have been failing for over an hour."),
    ).toBeInTheDocument();
    expect(screen.queryByText(/Federation with a\.down\.example/)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "See the failing servers" })).toBeInTheDocument();
  });

  it("names each server failing for over an hour when there are few", async () => {
    renderDashboard();
    expect(
      await screen.findByText("Federation with kde.org has been failing for over an hour."),
    ).toBeInTheDocument();
    expect(
      screen.getByText("Federation with mozilla.org has been failing for over an hour."),
    ).toBeInTheDocument();
    expect(screen.queryByText(/servers have been failing/)).not.toBeInTheDocument();
  });

  it("uses the failing list's own total when the Overview's counts cannot be read", async () => {
    server.use(
      http.get("/api/v1/statistics/overview", notImplemented),
      http.get("/api/v1/federation/destinations", ({ request }) => {
        const q = new URL(request.url).searchParams;
        const total = q.get("failing") === "true" ? 2 : 63;
        return HttpResponse.json({ items: [], next_cursor: null, prev_cursor: null, total });
      }),
    );
    renderDashboard();
    expect((await tile("Failing")).getByText("2")).toBeInTheDocument();
    expect((await tile("Not failing")).getByText("63")).toBeInTheDocument();
  });

  it("shows each of the server's health checks in words, and an ok server gives no row", async () => {
    server.use(
      http.get("/api/v1/server/health", () =>
        HttpResponse.json({ status: "ok", checks: { audit: "ok", events: "ok", users: "ok" } }),
      ),
    );
    renderDashboard();
    const card = within(await screen.findByRole("region", { name: "Health" }));
    expect(
      await card.findByText("Every probe answered: audit log, event stream, user directory."),
    ).toBeInTheDocument();
    expect(card.getByText("Audit log")).toBeInTheDocument();
    expect(card.getByText("Event stream")).toBeInTheDocument();
    expect(card.getByText("User directory")).toBeInTheDocument();
    expect(card.getAllByText("Ok")).toHaveLength(4);
    expect(screen.queryByText(/Server health is degraded/)).not.toBeInTheDocument();
    // An ok server keeps the rows behind a closed disclosure: the sentence already said it all.
    const details = card.getByText("Show the checks").closest("details");
    expect(details).not.toHaveAttribute("open");
  });

  it("opens the checks by itself when one of them is not ok", async () => {
    server.use(
      http.get("/api/v1/server/health", () =>
        HttpResponse.json({ status: "down", checks: { audit: "ok", events: "down", users: "ok" } }),
      ),
    );
    renderDashboard();
    const card = within(await screen.findByRole("region", { name: "Health" }));
    const details = (await card.findByText("The checks")).closest("details");
    expect(details).toHaveAttribute("open");
  });

  it("names a check the server cannot vouch for, and puts the degraded server under Attention", async () => {
    server.use(
      http.get("/api/v1/server/health", () =>
        HttpResponse.json({
          status: "degraded",
          checks: { audit: "ok", events: "ok", users: "unknown" },
        }),
      ),
    );
    renderDashboard();
    expect(
      await screen.findAllByText("Server health is degraded: user directory unknown."),
    ).toHaveLength(2);
    expect(screen.getByText(/not wired up here/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "See the checks" })).toBeInTheDocument();
  });

  it("says when server health could not be read, and does not give the all-clear", async () => {
    server.use(
      http.get("/api/v1/server/health", notImplemented),
      http.get("/api/v1/statistics/overview", () =>
        HttpResponse.json({ users_count: 3, rooms_count: 2, pending_reports_count: 0 }),
      ),
      http.get("/api/v1/appservices", () => HttpResponse.json(emptyPage)),
      http.get("/api/v1/federation/destinations", () => HttpResponse.json(emptyPage)),
      http.get("/api/v1/tasks", () => HttpResponse.json(emptyPage)),
    );
    renderDashboard();
    expect(await screen.findByText(/It can.t check server health yet/)).toBeInTheDocument();
  });

  it("says so when a task failed in the last day, and links to it", async () => {
    renderDashboard();

    const row = await screen.findByText(/Task "Delete room" failed: the room's owner replica/);
    expect(row).toBeInTheDocument();
    expect(screen.getAllByRole("button", { name: "Open task" })).toHaveLength(1);
  });

  it("gives the last seven days as sparklines, each with its number", async () => {
    server.use(
      http.get("/api/v1/statistics/timeseries", ({ request }) => {
        const metric = new URL(request.url).searchParams.get("metric");
        const values = metric === "daily_active_users" ? [180, 190, 214] : [1, 0, 2];
        return HttpResponse.json({
          metric,
          step_ms: 21_600_000,
          points: values.map((value, i) => ({
            at: new Date(Date.UTC(2026, 8, 20, i * 6)).toISOString(),
            value,
          })),
        });
      }),
    );
    renderDashboard();

    // A gauge shows its latest sample; a counter the total over the week.
    expect(await (await tile("Daily active, 7-day trend")).findByText("214")).toBeInTheDocument();
    expect(await (await tile("New accounts, 7 days")).findByText("3")).toBeInTheDocument();
    expect(await (await tile("Media uploaded, 7 days")).findByText("3 B")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "All statistics" })).toHaveAttribute(
      "href",
      "/statistics",
    );
  });

  it("marks an activity tile the server cannot answer, and keeps the rest", async () => {
    server.use(http.get("/api/v1/statistics/timeseries", notImplemented));
    renderDashboard();

    expect(
      await (await tile("New accounts, 7 days")).findByText("Not implemented"),
    ).toBeInTheDocument();
    expect((await tile("Users")).getByText("642")).toBeInTheDocument();
  });

  it("shows a dash, not a zero, for a count the server did not send", async () => {
    server.use(
      http.get("/api/v1/statistics/overview", () => HttpResponse.json({ users_count: 7 })),
    );
    renderDashboard();

    expect((await tile("Users")).getByText("7")).toBeInTheDocument();
    expect((await tile("Rooms")).getByText("—")).toBeInTheDocument();
    expect((await tile("Daily active users")).getByText("—")).toBeInTheDocument();
  });
});
