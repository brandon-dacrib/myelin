import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  RouterProvider,
  type AnyRouter,
} from "@tanstack/react-router";
import { Sidebar } from "./Sidebar";
import { signIn, signOut } from "@/lib/auth";

function renderSidebar(variant: "full" | "rail") {
  const rootRoute = createRootRoute();
  const router = createRouter({
    routeTree: rootRoute.addChildren([
      createRoute({
        getParentRoute: () => rootRoute,
        path: "/",
        component: () => <Sidebar variant={variant} />,
      }),
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

beforeEach(async () => {
  await signIn();
});

afterEach(() => {
  signOut();
});

describe("Sidebar", () => {
  it("groups the sections under Manage, Watch and Server, with the Overview above them", async () => {
    renderSidebar("full");
    const nav = await screen.findByRole("navigation", { name: "Primary" });
    const text = nav.textContent ?? "";
    // The order on the page: Overview, then the three headings each before its sections.
    const positions = [
      "Overview",
      "Manage",
      "Users",
      "Watch",
      "Federation",
      "Server",
      "Configuration",
    ].map((word) => text.indexOf(word));
    expect(positions.every((p) => p >= 0)).toBe(true);
    expect([...positions].sort((a, b) => a - b)).toEqual(positions);
  });

  it("calls the invite links, API tokens and notices section by what it holds", async () => {
    renderSidebar("full");
    expect(await screen.findByRole("link", { name: "Invites and tokens" })).toHaveAttribute(
      "href",
      "/settings",
    );
    expect(screen.queryByRole("link", { name: "Settings" })).not.toBeInTheDocument();
  });

  it("shows no group headings on the icon rail, but the groups are still named for a screen reader", async () => {
    renderSidebar("rail");
    await screen.findByRole("link", { name: "Overview" });
    expect(screen.queryByText("Manage")).not.toBeInTheDocument();
    expect(screen.queryByText("Server")).not.toBeInTheDocument();
    expect(screen.getByRole("group", { name: "Manage" })).toBeInTheDocument();
  });
});
