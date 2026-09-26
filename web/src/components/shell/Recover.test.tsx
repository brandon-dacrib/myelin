import { afterEach, describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
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
import { inspectRecoveryLink } from "@/lib/recovery";
import { MOCK_RECOVERY_TOKEN, resetMockRecovery, useMockRecoveryLink } from "@/mocks/data/recovery";

/**
 * The real `AppShell` at the real base path, as `Setup.test.tsx` does: what is under test is as
 * much "which of sign-in, recovery and the app does this address show" as the form itself.
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
        path: "/recover",
        component: () => <Navigate to="/" replace />,
      }),
    ]),
    basepath: "/admin",
    history: createMemoryHistory({ initialEntries: [address] }),
  }) as unknown as AnyRouter;
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return router;
}

const HEADING = "Recover administrator access";
const SUBMIT = "Reset password and sign in";

async function fillPassword(password: string, confirm = password) {
  await userEvent.type(screen.getByLabelText(/^New password/), password);
  await userEvent.type(screen.getByLabelText(/^Confirm password/), confirm);
}

afterEach(() => {
  signOut();
  resetMockRecovery();
});

describe("administrator recovery page", () => {
  it("shows the administrators the link can reset, and how long it has", async () => {
    renderAt(`/admin/recover#token=${MOCK_RECOVERY_TOKEN}`);

    await screen.findByRole("heading", { name: HEADING });
    const group = await screen.findByRole("group", { name: /^Account/ });
    expect(group).toBeInTheDocument();
    expect(screen.getByRole("radio", { name: "@admin:example.org" })).not.toBeChecked();
    expect(screen.getByRole("radio", { name: "@ops:example.org" })).not.toBeChecked();
    expect(screen.getByText("This link works once and expires in 14 minutes.")).toBeInTheDocument();
    expect(
      screen.getByText(/every other session of that account will be signed out/),
    ).toBeVisible();
  });

  it("reads the token from the fragment only, never the query string", async () => {
    renderAt(`/admin/recover?token=${MOCK_RECOVERY_TOKEN}`);

    await screen.findByRole("heading", { name: HEADING });
    expect(await screen.findByText(/needs the link that/)).toBeInTheDocument();
    expect(screen.getByText("hs recover")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: SUBMIT })).not.toBeInTheDocument();

    await userEvent.click(screen.getByRole("button", { name: "Go to sign in" }));
    await screen.findByRole("button", { name: "Sign in" });
  });

  it("says a wrong token is not this server's link", async () => {
    renderAt("/admin/recover#token=not-the-token");

    expect(
      await screen.findByText(
        "This is not this server's recovery link. Check the link you were given.",
      ),
    ).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: SUBMIT })).not.toBeInTheDocument();
  });

  it("says no link is open once it has been used", async () => {
    useMockRecoveryLink();
    renderAt(`/admin/recover#token=${MOCK_RECOVERY_TOKEN}`);

    const alert = await screen.findByRole("alert");
    expect(alert).toHaveTextContent(
      "No recovery link is open. Run hs recover where the server keeps its signing key to get a fresh one.",
    );
    expect(screen.getByText("hs recover")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: SUBMIT })).not.toBeInTheDocument();
  });

  it("asks for an account before asking the server anything", async () => {
    renderAt(`/admin/recover#token=${MOCK_RECOVERY_TOKEN}`);
    await screen.findByRole("group", { name: /^Account/ });

    await fillPassword("hunter2-ops");
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));

    expect(await screen.findByText("Choose the account to recover.")).toBeInTheDocument();
    expect(getSession()).toBeNull();
  });

  it("catches mismatched passwords before asking the server anything", async () => {
    renderAt(`/admin/recover#token=${MOCK_RECOVERY_TOKEN}`);
    await screen.findByRole("group", { name: /^Account/ });

    await userEvent.click(screen.getByRole("radio", { name: "@admin:example.org" }));
    await fillPassword("hunter2-ops", "hunter2-oops");
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));

    expect(await screen.findByText("The two passwords don’t match.")).toBeInTheDocument();
    expect(screen.getByLabelText(/^New password/)).toHaveAttribute("aria-invalid", "true");
    expect(getSession()).toBeNull();
    // The link is still open.
    await expect(inspectRecoveryLink(MOCK_RECOVERY_TOKEN)).resolves.toBeTruthy();
  });

  it("shows the server's password policy beside the password field", async () => {
    renderAt(`/admin/recover#token=${MOCK_RECOVERY_TOKEN}`);
    await screen.findByRole("group", { name: /^Account/ });

    await userEvent.click(screen.getByRole("radio", { name: "@admin:example.org" }));
    await fillPassword("short");
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));

    expect(await screen.findByText(/Password too short/)).toBeInTheDocument();
    expect(screen.getByLabelText(/^New password/)).toHaveAttribute("aria-invalid", "true");
    // The form is still there to try again with; the link was not spent.
    expect(screen.getByRole("button", { name: SUBMIT })).toBeInTheDocument();
    expect(getSession()).toBeNull();
  });

  it("resets the password, stores the session and leaves the page", async () => {
    const router = renderAt(`/admin/recover#token=${MOCK_RECOVERY_TOKEN}`);
    await screen.findByRole("group", { name: /^Account/ });

    await userEvent.click(screen.getByRole("radio", { name: "@ops:example.org" }));
    await fillPassword("hunter2-ops");
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));

    await screen.findByRole("heading", { name: "Overview" });
    expect(getSession()?.scopes).toContain("admin:write");
    // The token is gone from the address, and the link is spent.
    expect(router.state.location.href).not.toContain(MOCK_RECOVERY_TOKEN);
    expect(router.state.location.pathname).not.toContain("recover");
    await expect(inspectRecoveryLink(MOCK_RECOVERY_TOKEN)).rejects.toMatchObject({
      refusal: "no-link-open",
    });
  });

  it("says so in place of the form when the link is used under it", async () => {
    renderAt(`/admin/recover#token=${MOCK_RECOVERY_TOKEN}`);
    await screen.findByRole("group", { name: /^Account/ });

    await userEvent.click(screen.getByRole("radio", { name: "@admin:example.org" }));
    await fillPassword("hunter2-ops");
    // Another tab used the link while this form was open.
    useMockRecoveryLink();
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));

    expect(await screen.findByRole("alert")).toHaveTextContent(/No recovery link is open/);
    expect(screen.queryByRole("button", { name: SUBMIT })).not.toBeInTheDocument();
    expect(getSession()).toBeNull();
  });
});

describe("sign-in page", () => {
  it("tells a locked-out operator where a recovery link comes from", async () => {
    renderAt("/admin/");

    await screen.findByRole("button", { name: "Sign in" });
    expect(screen.getByText(/Locked out\?/)).toHaveTextContent(
      "Locked out? Run hs recover where the server runs to get a recovery link.",
    );
  });
});
