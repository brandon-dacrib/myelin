import { afterEach, describe, expect, it } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  createMemoryHistory,
  createRootRoute,
  createRoute,
  createRouter,
  Navigate,
  RouterProvider,
  type AnyRouter,
} from "@tanstack/react-router";
import { AppShell } from "./AppShell";
import { getSession, signOut } from "@/lib/auth";
import { fetchNeedsSetup } from "@/lib/setup";
import { MOCK_SETUP_TOKEN_KEY } from "@/mocks/handlers";

const TOKEN = "mockSetupTokenMockSetupTokenMockSetupTok";

/**
 * The real `AppShell` at the real base path, because what is under test is as much "which of
 * sign-in, setup and the app does this address show" as the form itself. Only the pages behind
 * it are stand-ins.
 */
function renderAt(address: string) {
  const rootRoute = createRootRoute({ component: AppShell });
  const router = createRouter({
    routeTree: rootRoute.addChildren([
      createRoute({
        getParentRoute: () => rootRoute,
        path: "/",
        component: () => <h1>Overview</h1>,
      }),
      createRoute({
        getParentRoute: () => rootRoute,
        path: "/setup",
        component: () => <Navigate to="/" replace />,
      }),
    ]),
    basepath: "/admin",
    history: createMemoryHistory({ initialEntries: [address] }),
  }) as unknown as AnyRouter;
  // The signed-in shell (top bar, command palette) reads through react-query.
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return router;
}

async function fillAccount(username: string, password: string, confirm = password) {
  await userEvent.type(screen.getByLabelText(/^Username/), username);
  await userEvent.type(screen.getByLabelText(/^Password/), password);
  await userEvent.type(screen.getByLabelText(/^Confirm password/), confirm);
}

afterEach(() => {
  signOut();
  sessionStorage.removeItem(MOCK_SETUP_TOKEN_KEY);
});

describe("first-run setup page", () => {
  it("takes someone who followed the setup link from nothing to signed in", async () => {
    sessionStorage.setItem(MOCK_SETUP_TOKEN_KEY, TOKEN);
    const router = renderAt(`/admin/setup#token=${TOKEN}`);

    await screen.findByRole("heading", { name: "Create the first administrator" });
    // The link already carried the token, so it is not asked for again.
    expect(screen.queryByLabelText(/^Setup token/)).not.toBeInTheDocument();

    await fillAccount("ops", "hunter2-ops");
    await userEvent.click(screen.getByRole("button", { name: "Create administrator" }));

    await screen.findByRole("heading", { name: "Overview" });
    expect(getSession()?.scopes).toContain("admin:write");
    // The token is gone from the address, and the server's offer is over.
    expect(router.state.location.href).not.toContain(TOKEN);
    expect(router.state.location.pathname).not.toContain("setup");
    expect(await fetchNeedsSetup()).toBe(false);
  });

  it("asks for the token when the address has none, and accepts the whole link", async () => {
    sessionStorage.setItem(MOCK_SETUP_TOKEN_KEY, TOKEN);
    renderAt("/admin/setup");

    const tokenField = await screen.findByLabelText(/^Setup token/);
    await userEvent.type(tokenField, `http://localhost:8008/admin/setup#token=${TOKEN}`);
    await fillAccount("ops", "hunter2-ops");
    await userEvent.click(screen.getByRole("button", { name: "Create administrator" }));

    await screen.findByRole("heading", { name: "Overview" });
  });

  it("catches mismatched passwords before asking the server anything", async () => {
    sessionStorage.setItem(MOCK_SETUP_TOKEN_KEY, TOKEN);
    renderAt(`/admin/setup#token=${TOKEN}`);
    await screen.findByRole("heading", { name: "Create the first administrator" });

    await fillAccount("ops", "hunter2-ops", "hunter2-oops");
    await userEvent.click(screen.getByRole("button", { name: "Create administrator" }));

    expect(await screen.findByText("The two passwords don’t match.")).toBeInTheDocument();
    expect(screen.getByLabelText(/^Password/)).toHaveAttribute("aria-invalid", "true");
    expect(getSession()).toBeNull();
    expect(await fetchNeedsSetup()).toBe(true);
  });

  it("shows the server's reason beside the field it refused", async () => {
    sessionStorage.setItem(MOCK_SETUP_TOKEN_KEY, TOKEN);
    renderAt(`/admin/setup#token=${TOKEN}`);
    await screen.findByRole("heading", { name: "Create the first administrator" });

    await fillAccount("ops", "short");
    await userEvent.click(screen.getByRole("button", { name: "Create administrator" }));

    expect(await screen.findByText(/Password too short/)).toBeInTheDocument();
    expect(screen.getByLabelText(/^Password/)).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByLabelText(/^Username/)).not.toHaveAttribute("aria-invalid");
  });

  it("says so, and offers sign-in, when the server already has its administrator", async () => {
    renderAt(`/admin/setup#token=${TOKEN}`);

    await screen.findByRole("heading", { name: "This server is already set up" });
    expect(screen.queryByRole("button", { name: "Create administrator" })).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Go to sign in" }));
    await screen.findByRole("button", { name: "Sign in" });
  });
});

describe("sign-in while the server has no administrator", () => {
  it("explains why nobody can sign in yet, and links to setup", async () => {
    sessionStorage.setItem(MOCK_SETUP_TOKEN_KEY, TOKEN);
    renderAt("/admin/");

    expect(await screen.findByText("This server has no administrator yet")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("link", { name: "enter the setup token here" }));
    await screen.findByRole("heading", { name: "Create the first administrator" });
    expect(screen.getByLabelText(/^Setup token/)).toBeInTheDocument();
  });

  it("says nothing of the kind on a server that is set up", async () => {
    renderAt("/admin/");
    await screen.findByRole("button", { name: "Sign in" });
    await waitFor(async () => expect(await fetchNeedsSetup()).toBe(false));
    expect(screen.queryByText("This server has no administrator yet")).not.toBeInTheDocument();
  });
});
