import { afterEach, describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
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
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { AppShell } from "./AppShell";
import { signIn, signOut } from "@/lib/auth";
import { findRegistrationToken } from "@/mocks/data/registration-tokens";
import { findUser, users } from "@/mocks/data/users";

/**
 * The real `AppShell` at the real base path, as `Recover.test.tsx` does: what is under test is as
 * much "an invite link shows the registration page, signed in or not" as the form itself.
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
        path: "/register",
        component: () => null,
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
}

const HEADING = "Create your account";
const SUBMIT = "Create account";
const before = users.length;

async function fill(username: string, password: string, confirm = password) {
  await userEvent.type(await screen.findByLabelText(/^Username/), username);
  await userEvent.type(screen.getByLabelText(/^Password/), password);
  await userEvent.type(screen.getByLabelText(/^Confirm password/), confirm);
}

afterEach(() => {
  users.length = before;
  signOut();
});

describe("invite-link registration page", () => {
  it("registers an account with the link's token and says where to sign in", async () => {
    const bodies: unknown[] = [];
    server.use(
      http.post("*/_matrix/client/v3/register", async ({ request }) => {
        bodies.push(await request.clone().json());
      }),
    );
    renderAt("/admin/register?token=carol-invite");

    await screen.findByRole("heading", { name: HEADING });
    await fill("Carol", "correct-horse-battery");
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));

    expect(await screen.findByRole("heading", { name: "Your account is ready" })).toBeVisible();
    expect(screen.getByText("@carol:example.org")).toBeInTheDocument();
    expect(screen.getByText(/Sign in with any Matrix client/)).toHaveTextContent(
      window.location.origin,
    );
    expect(bodies[0]).toEqual({
      username: "carol",
      password: "correct-horse-battery",
      inhibit_login: true,
      auth: { type: "m.login.registration_token", token: "carol-invite" },
    });
    expect(findUser("@carol:example.org")).toBeDefined();
    expect(findRegistrationToken("carol-invite")?.completed).toBe(1);
  });

  it("works the same while an administrator is signed in to this browser", async () => {
    await signIn();
    renderAt("/admin/register?token=welcome-team");

    expect(await screen.findByRole("heading", { name: HEADING })).toBeInTheDocument();
    expect(screen.queryByRole("navigation")).not.toBeInTheDocument();
    await fill("frank", "correct-horse-battery");
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));
    expect(await screen.findByText("@frank:example.org")).toBeInTheDocument();
  });

  it("says a used-up or expired link is no longer valid, before asking for anything", async () => {
    renderAt("/admin/register?token=dave-invite");

    expect(await screen.findByRole("alert")).toHaveTextContent(
      /This invite link is no longer valid/,
    );
    expect(screen.queryByRole("button", { name: SUBMIT })).not.toBeInTheDocument();
  });

  it("says a link without a token is not an invite", async () => {
    renderAt("/admin/register");

    expect(await screen.findByRole("alert")).toHaveTextContent(/This page needs an invite link/);
  });

  it("puts a taken username beside the username field", async () => {
    renderAt("/admin/register?token=carol-invite");

    await fill("alice", "correct-horse-battery");
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));

    expect(await screen.findByText("That username is taken. Try another.")).toBeInTheDocument();
    expect(screen.getByLabelText(/^Username/)).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByLabelText(/^Password/)).not.toHaveAttribute("aria-invalid");
    expect(findRegistrationToken("carol-invite")?.completed).toBe(0);
  });

  it("checks the username as soon as it is typed", async () => {
    renderAt("/admin/register?token=carol-invite");

    await userEvent.type(await screen.findByLabelText(/^Username/), "alice");
    await userEvent.tab();
    expect(await screen.findByText("That username is taken. Try another.")).toBeInTheDocument();

    await userEvent.clear(screen.getByLabelText(/^Username/));
    await userEvent.type(screen.getByLabelText(/^Username/), "grace");
    await userEvent.tab();
    expect(await screen.findByText("Available.")).toBeInTheDocument();
  });

  it("puts a weak password, and a mismatched one, beside the password fields", async () => {
    renderAt("/admin/register?token=carol-invite");

    await fill("grace", "hunter2-grace", "hunter2-graze");
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));
    expect(await screen.findByText("The two passwords don’t match.")).toBeInTheDocument();
    expect(screen.getByLabelText(/^Confirm password/)).toHaveAttribute("aria-invalid", "true");

    await userEvent.clear(screen.getByLabelText(/^Password/));
    await userEvent.clear(screen.getByLabelText(/^Confirm password/));
    await userEvent.type(screen.getByLabelText(/^Password/), "short");
    await userEvent.type(screen.getByLabelText(/^Confirm password/), "short");
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));
    expect(await screen.findByText(/Password too short/)).toBeInTheDocument();
    expect(screen.getByLabelText(/^Password/)).toHaveAttribute("aria-invalid", "true");
  });

  it("says so in place of the form when the token stops working under it", async () => {
    renderAt("/admin/register?token=carol-invite");
    await fill("grace", "correct-horse-battery");
    // Somebody else used the last use while this form was open.
    findRegistrationToken("carol-invite")!.completed = 1;
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      /This invite link is no longer valid/,
    );
    expect(screen.queryByRole("button", { name: SUBMIT })).not.toBeInTheDocument();
  });

  it("finishes with m.login.dummy when that is the only stage left", async () => {
    const auths: unknown[] = [];
    server.use(
      http.post("*/_matrix/client/v3/register", async ({ request }) => {
        const body = (await request.json()) as { auth?: { type?: string } };
        auths.push(body.auth);
        if (body.auth?.type === "m.login.registration_token") {
          return HttpResponse.json(
            {
              session: "uia-1",
              flows: [{ stages: ["m.login.registration_token", "m.login.dummy"] }],
              completed: ["m.login.registration_token"],
              params: {},
            },
            { status: 401 },
          );
        }
        return HttpResponse.json({ user_id: "@grace:example.org" });
      }),
    );
    renderAt("/admin/register?token=carol-invite");
    await fill("grace", "correct-horse-battery");
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));

    expect(await screen.findByText("@grace:example.org")).toBeInTheDocument();
    expect(auths).toEqual([
      { type: "m.login.registration_token", token: "carol-invite" },
      { type: "m.login.dummy", session: "uia-1" },
    ]);
  });

  it("says plainly when the server wants a step this page cannot do", async () => {
    server.use(
      http.post("*/_matrix/client/v3/register", () =>
        HttpResponse.json(
          {
            session: "uia-2",
            flows: [{ stages: ["m.login.registration_token", "m.login.email.identity"] }],
            completed: ["m.login.registration_token"],
          },
          { status: 401 },
        ),
      ),
    );
    renderAt("/admin/register?token=carol-invite");
    await fill("grace", "correct-horse-battery");
    await userEvent.click(screen.getByRole("button", { name: SUBMIT }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      /asks for more than an invite link to register/,
    );
  });
});
