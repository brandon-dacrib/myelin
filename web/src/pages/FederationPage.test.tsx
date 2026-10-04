import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { federationDestinations } from "@/mocks/data/dashboard";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { FederationPage } from "./FederationPage";
import { validateFederationSearch } from "./federation/federation-search";

/**
 * The Federation page at scale: the server filters, orders and pages the list, and the page
 * shows the server's totals. The mock has 65 destinations (`src/mocks/data/dashboard.ts`), so
 * the list has two pages of fifty.
 */
function open(path = "/federation") {
  return renderRoutes(
    [{ path: "/federation", component: FederationPage, validateSearch: validateFederationSearch }],
    path,
    ["/federation/$serverName", "/configuration/$section"],
  );
}

async function serverNames(): Promise<string[]> {
  const table = await screen.findByRole("table", { name: "Federation destinations" });
  return within(table)
    .getAllByRole("row")
    .slice(1)
    .map((row) => within(row).getAllByRole("link")[0].textContent ?? "");
}

describe("Federation at scale", () => {
  beforeEach(async () => {
    await signIn();
  });
  afterEach(() => {
    server.resetHandlers();
    signOut();
  });

  it("lists the server's first page in the server's order, failing first, with the whole count", async () => {
    open();
    const names = await serverNames();
    expect(names).toHaveLength(50);
    expect(names.slice(0, 5)).toEqual([
      "kde.org",
      "mozilla.org",
      "element.io",
      "gnome.org",
      "matrix.org",
    ]);
    expect(screen.getByText("65 servers")).toBeInTheDocument();
    expect(screen.getByText("Failing servers first, then the rest by name.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Next" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Previous" })).toBeDisabled();
  });

  it("pages with the server's cursor, kept in the URL", async () => {
    const { router } = open();
    await serverNames();
    await userEvent.click(screen.getByRole("button", { name: "Next" }));
    await waitFor(() => expect(router.state.location.search).toHaveProperty("cursor"));
    await waitFor(async () => expect(await serverNames()).toHaveLength(15));
    expect(await serverNames()).toContain("srv-60.example.net");
    expect(screen.getByRole("button", { name: "Previous" })).toBeEnabled();
    expect(screen.getByRole("button", { name: "Next" })).toBeDisabled();
  });

  it("filters to the failing servers, longest failing first, and says how many there are", async () => {
    const { router } = open();
    await serverNames();
    await userEvent.click(screen.getByRole("button", { name: "Failing" }));
    await waitFor(() => expect(router.state.location.search).toEqual({ show: "failing" }));
    await waitFor(async () => expect(await serverNames()).toEqual(["kde.org", "mozilla.org"]));
    expect(screen.getByText("2 servers failing")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Failing" })).toHaveAttribute("aria-pressed", "true");

    await userEvent.click(screen.getByRole("button", { name: "Not failing" }));
    await waitFor(() => expect(router.state.location.search).toEqual({ show: "not-failing" }));
    await waitFor(async () => expect(await serverNames()).toHaveLength(50));
    expect(screen.getByText("63 servers not failing")).toBeInTheDocument();
    expect(await serverNames()).not.toContain("kde.org");
  });

  it("sorts by a column through the server, and turns the order round on a second click", async () => {
    const { router } = open();
    await serverNames();
    await userEvent.click(screen.getByRole("button", { name: /Last success/ }));
    await waitFor(() =>
      expect(router.state.location.search).toEqual({ sort: "last_successful_at" }),
    );
    // Ascending: the oldest success first, which is the server down for two days.
    await waitFor(async () => expect((await serverNames())[0]).toBe("kde.org"));
    expect(screen.getByText(/Sorted by the column you chose/)).toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: /Last success/ }));
    await waitFor(() =>
      expect(router.state.location.search).toEqual({ sort: "-last_successful_at" }),
    );
    await waitFor(async () => expect((await serverNames())[0]).toBe("matrix.org"));
  });

  it("drops a sort field the server does not know rather than sending it", async () => {
    const { router } = open("/federation?sort=bogus&show=failing");
    await serverNames();
    expect(router.state.location.search).toEqual({ show: "failing" });
  });

  it("says when a filter matches nothing, in the filter's own words", async () => {
    server.use(
      http.get("/api/v1/federation/destinations", ({ request }) => {
        const url = new URL(request.url);
        const failing = url.searchParams.get("failing");
        const items = failing === "true" ? [] : federationDestinations.slice(0, 3);
        return HttpResponse.json({
          items,
          next_cursor: null,
          prev_cursor: null,
          total: items.length,
        });
      }),
    );
    open("/federation?show=failing");
    expect(await screen.findByText("No failing servers")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Failing" })).toHaveAttribute("aria-pressed", "true");
  });

  it("shows the server's refusal when the list cannot be read", async () => {
    server.use(
      http.get("/api/v1/federation/destinations", () =>
        HttpResponse.json(
          { type: "urn:hs:problem:unavailable", title: "Unavailable", status: 503 },
          { status: 503 },
        ),
      ),
    );
    open();
    expect(await screen.findByText(/federation destinations/i)).toBeInTheDocument();
    expect(
      screen.queryByRole("table", { name: "Federation destinations" }),
    ).not.toBeInTheDocument();
  });
});
