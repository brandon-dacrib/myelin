import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { StatisticsPage } from "./StatisticsPage";
import { validateStatisticsSearch } from "./statistics-search";

function open(path = "/statistics") {
  return renderRoutes(
    [{ path: "/statistics", component: StatisticsPage, validateSearch: validateStatisticsSearch }],
    path,
    ["/rooms/$roomId", "/users/$userId"],
  );
}

async function card(title: string) {
  const heading = await screen.findByRole("heading", { name: title, level: 3 });
  return within(heading.closest("section") as HTMLElement);
}

/** Answers every series with the same three points, and records what was asked. */
function fixedSeries(asked: URLSearchParams[], empty: string[] = []) {
  server.use(
    http.get("/api/v1/statistics/timeseries", ({ request }) => {
      const params = new URL(request.url).searchParams;
      asked.push(params);
      const metric = params.get("metric")!;
      const points = empty.includes(metric)
        ? []
        : [5, 0, 7].map((value, i) => ({
            at: new Date(Date.now() - (3 - i) * 21_600_000).toISOString(),
            value,
          }));
      return HttpResponse.json({ metric, step_ms: 21_600_000, points });
    }),
  );
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("Statistics", () => {
  it("shows the server's counts now", async () => {
    open();
    const now = within(
      (await screen.findByRole("heading", { name: "Now" })).closest("section") as HTMLElement,
    );
    expect(await now.findByText("642")).toBeInTheDocument();
    expect(now.getByText("38.0 GiB")).toBeInTheDocument();
  });

  it("charts each metric over the last seven days, a total for counters and the latest for gauges", async () => {
    const asked: URLSearchParams[] = [];
    fixedSeries(asked);
    open();

    const accounts = await card("New accounts");
    expect(await accounts.findByText("12")).toBeInTheDocument();
    expect(accounts.getByText("last 7 days")).toBeInTheDocument();
    expect(accounts.getByRole("img")).toHaveAccessibleName(/3 points; highest 7; latest 7/);

    const dau = await card("Daily active users");
    expect(await dau.findByText("latest")).toBeInTheDocument();

    const media = await card("Media uploaded");
    expect(await media.findByText("12 B")).toBeInTheDocument();

    await waitFor(() => expect(asked.length).toBeGreaterThanOrEqual(6));
    const accountsAsked = asked.find((p) => p.get("metric") === "users.registered")!;
    expect(accountsAsked.get("step")).toBe("6h");
    const span = Date.parse(accountsAsked.get("until")!) - Date.parse(accountsAsked.get("from")!);
    expect(span).toBe(7 * 86_400_000);
  });

  it("gives the same numbers as a table", async () => {
    const user = userEvent.setup();
    fixedSeries([]);
    open();
    const reports = await card("Reports received");
    await user.click(await reports.findByRole("button", { name: "Show as table" }));
    const table = within(reports.getByRole("table"));
    expect(table.getAllByRole("row")).toHaveLength(4);
    expect(table.getByText("7")).toBeInTheDocument();
  });

  it("asks for the range in the URL", async () => {
    const asked: URLSearchParams[] = [];
    fixedSeries(asked);
    open("/statistics?range=30d");
    await card("New accounts");
    await waitFor(() => expect(asked.length).toBeGreaterThan(0));
    expect(asked.every((p) => p.get("step") === "1d")).toBe(true);
  });

  it("says a gauge has no history yet rather than drawing zeros", async () => {
    fixedSeries([], ["users_count"]);
    open();
    const accounts = await card("Accounts");
    expect(await accounts.findByText("No data in this range yet.")).toBeInTheDocument();
    expect(accounts.getByText("—")).toBeInTheDocument();
  });

  it("keeps the other charts when one metric cannot be answered", async () => {
    server.use(
      http.get("/api/v1/statistics/timeseries", ({ request }) =>
        new URL(request.url).searchParams.get("metric") === "reports.received"
          ? HttpResponse.json(
              { type: "urn:hs:problem:unavailable", title: "Unavailable", status: 503 },
              { status: 503 },
            )
          : undefined,
      ),
    );
    open();
    const reports = await card("Reports received");
    expect(
      await reports.findByText(
        "Reports received isn't connected to a data source on this server yet",
      ),
    ).toBeInTheDocument();
    const accounts = await card("New accounts");
    expect(await accounts.findByRole("img")).toBeInTheDocument();
  });

  it("lists the largest rooms and sorts them by what the operator picks", async () => {
    const user = userEvent.setup();
    const sorts: (string | null)[] = [];
    server.use(
      http.get("/api/v1/statistics/rooms", ({ request }) => {
        sorts.push(new URL(request.url).searchParams.get("sort"));
        return undefined;
      }),
    );
    const { router } = open();
    const rooms = within(await screen.findByRole("table", { name: "Largest rooms" }));
    const rows = rooms.getAllByRole("row").slice(1);
    expect(within(rows[0]).getByText("Announcements")).toBeInTheDocument();
    expect(within(rows[0]).getByText("598")).toBeInTheDocument();
    expect(sorts[0]).toBe("-joined_members_count");

    await user.click(rooms.getByRole("button", { name: /State events/ }));
    await waitFor(() => expect(sorts.at(-1)).toBe("state_events_count"));
    expect(router.state.location.search).toMatchObject({ rooms_sort: "state_events_count" });
  });

  it("lists whose media takes the space", async () => {
    open();
    const media = within(await screen.findByRole("table", { name: "Media by person" }));
    const rows = media.getAllByRole("row").slice(1);
    expect(within(rows[0]).getByText("@alice:example.org")).toBeInTheDocument();
    expect(within(rows[0]).getByText("14.2 GiB")).toBeInTheDocument();
  });

  it("is not shown without admin:read", async () => {
    await signIn(["moderation:read"]);
    open();
    expect(await screen.findByText(/This needs the/)).toHaveTextContent("admin:read");
  });
});
