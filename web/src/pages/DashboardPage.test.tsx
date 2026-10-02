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
    expect((await tile("Mode")).getByText("Single node")).toBeInTheDocument();
    expect((await tile("Uptime")).getByText("5m")).toBeInTheDocument();
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
