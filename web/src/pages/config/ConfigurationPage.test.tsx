import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  RouterProvider,
  type AnyRouter,
} from "@tanstack/react-router";
import { ConfigurationPage } from "./ConfigurationPage";
import { Toaster } from "@/components/ui/toast/Toaster";
import { configLastReloaded } from "@/mocks/data/config";
import { signIn, signOut } from "@/lib/auth";

function renderIndex() {
  const rootRoute = createRootRoute();
  const indexRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/configuration",
    component: ConfigurationPage,
  });
  const sectionRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/configuration/$section",
    component: () => <p>Section</p>,
  });
  const router = createRouter({
    routeTree: rootRoute.addChildren([indexRoute, sectionRoute]),
    history: createMemoryHistory({ initialEntries: ["/configuration"] }),
  }) as unknown as AnyRouter;
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
      <Toaster />
    </QueryClientProvider>,
  );
}

beforeEach(async () => {
  await signIn(["admin:read", "admin:write"]);
});

afterEach(() => {
  signOut();
});

describe("ConfigurationPage", () => {
  it("lists every section the server reports", async () => {
    renderIndex();
    expect(await screen.findByRole("link", { name: "Rate limits" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Federation" })).toBeInTheDocument();
    expect(screen.getByRole("link", { name: "Storage" })).toBeInTheDocument();
    expect(screen.getAllByRole("link").length).toBeGreaterThanOrEqual(10);
  });

  it("says which sections reload and which need a restart", async () => {
    renderIndex();
    await screen.findByRole("link", { name: "Rate limits" });

    // The four reloadable sections, plus `storage`, which cannot be written at all.
    expect(screen.getAllByText("Reloadable")).toHaveLength(4);
    expect(screen.getByText("Bootstrap only")).toBeInTheDocument();
    expect(screen.getAllByText("Restart required").length).toBeGreaterThan(0);
  });

  it("counts what has been changed from default, and what the environment pins", async () => {
    renderIndex();
    const card = (await screen.findByRole("link", { name: "Server" })).closest("li")!;
    expect(within(card).getByText("1 pinned by environment")).toBeInTheDocument();
    expect(within(card).getByText(/of 6$/)).toBeInTheDocument();
  });

  it("searches across every setting in every section", async () => {
    const user = userEvent.setup();
    renderIndex();
    await screen.findByRole("link", { name: "Rate limits" });

    await user.type(screen.getByLabelText("Search settings"), "registration");

    // `auth.enable_registration` is in a different section from
    // `rate_limits.registration`; both match, nothing else does.
    expect(await screen.findByText("auth.enable_registration")).toBeInTheDocument();
    expect(screen.getByText("appservices.registration_files")).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: "Cluster" })).not.toBeInTheDocument();
  });

  it("says so when nothing matches", async () => {
    const user = userEvent.setup();
    renderIndex();
    await screen.findByRole("link", { name: "Rate limits" });

    await user.type(screen.getByLabelText("Search settings"), "zzzznope");
    expect(await screen.findByText("No settings match")).toBeInTheDocument();
  });

  it("re-reads the configuration files, and reports what was reloaded", async () => {
    const user = userEvent.setup();
    renderIndex();
    await screen.findByRole("link", { name: "Rate limits" });

    await user.click(screen.getByRole("button", { name: "Re-read files" }));
    const dialog = within(await screen.findByRole("dialog"));
    expect(
      dialog.getByText(/Settings stored in the database still win over the file/),
    ).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Re-read files" }));

    expect(await screen.findByText("Reloaded 4 sections")).toBeInTheDocument();
    await waitFor(() => expect(configLastReloaded.appservices).not.toBeNull());
  });
});
