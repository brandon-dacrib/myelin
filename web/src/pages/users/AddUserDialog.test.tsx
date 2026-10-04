import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
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
import { AddUserDialog } from "./AddUserDialog";
import { users } from "@/mocks/data/users";
import { signIn, signOut } from "@/lib/auth";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";

function renderDialog() {
  const onOpenChange = vi.fn();
  const rootRoute = createRootRoute();
  const router = createRouter({
    routeTree: rootRoute.addChildren([
      createRoute({
        getParentRoute: () => rootRoute,
        path: "/",
        component: () => <AddUserDialog open onOpenChange={onOpenChange} />,
      }),
      createRoute({
        getParentRoute: () => rootRoute,
        path: "/users/$userId",
        component: () => <p>Detail</p>,
      }),
    ]),
    history: createMemoryHistory({ initialEntries: ["/"] }),
  }) as unknown as AnyRouter;
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <RouterProvider router={router} />
    </QueryClientProvider>,
  );
  return { onOpenChange };
}

const before = users.length;

beforeEach(async () => {
  await signIn();
});

afterEach(() => {
  users.length = before;
  signOut();
});

describe("AddUserDialog", () => {
  it("creates the account and ends on what has to be handed over", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Add a user" }));

    await user.type(dialog.getByLabelText(/^Username/), "Carol");
    await user.type(dialog.getByLabelText(/^Display name/), "Carol D");
    await user.click(dialog.getByRole("button", { name: "Generate" }));

    // A generated password is shown: nobody can hand over a password they cannot read.
    const password = dialog.getByLabelText(/^Password/) as HTMLInputElement;
    expect(password.type).toBe("text");
    expect(password.value).toHaveLength(20);
    const generated = password.value;

    await user.click(dialog.getByRole("button", { name: "Create account" }));

    const done = within(await screen.findByRole("dialog", { name: "Account created" }));
    expect(done.getByText("@carol:example.org")).toBeInTheDocument();
    expect(done.getByText(generated)).toBeInTheDocument();
    // Copyable, by a button that says what it copies without saying the password itself.
    expect(done.getByRole("button", { name: "Copy password" })).toBeInTheDocument();
    expect(done.queryByRole("button", { name: new RegExp(generated) })).not.toBeInTheDocument();
    expect(done.getByText(/They can sign in from any Matrix client/)).toBeInTheDocument();

    const created = users.find((u) => u.user_id === "@carol:example.org");
    expect(created).toMatchObject({ display_name: "Carol D", admin: false });
  });

  it("says what an administrator account means, and makes one only when asked", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Add a user" }));

    const adminSwitch = dialog.getByRole("switch", { name: "Server administrator" });
    expect(adminSwitch).toHaveAttribute("aria-checked", "false");
    expect(adminSwitch).toHaveAccessibleDescription(/change anything on the server/);
    await user.click(adminSwitch);

    await user.type(dialog.getByLabelText(/^Username/), "ops2");
    await user.type(dialog.getByLabelText(/^Password/), "hunter2-ops2");
    await user.click(dialog.getByRole("button", { name: "Create account" }));

    const done = within(await screen.findByRole("dialog", { name: "Account created" }));
    expect(done.getByText(/They are a server administrator/)).toBeInTheDocument();
    expect(users.find((u) => u.user_id === "@ops2:example.org")?.admin).toBe(true);
  });

  it("puts a taken username, and a refused password, beside the right field", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Add a user" }));

    await user.type(dialog.getByLabelText(/^Username/), "alice");
    await user.type(dialog.getByLabelText(/^Password/), "hunter2-alice");
    await user.click(dialog.getByRole("button", { name: "Create account" }));
    expect(await dialog.findByText("@alice:example.org already exists")).toBeInTheDocument();
    expect(dialog.getByLabelText(/^Username/)).toHaveAttribute("aria-invalid", "true");

    await user.clear(dialog.getByLabelText(/^Username/));
    await user.type(dialog.getByLabelText(/^Username/), "dave");
    await user.clear(dialog.getByLabelText(/^Password/));
    await user.type(dialog.getByLabelText(/^Password/), "short");
    await user.click(dialog.getByRole("button", { name: "Create account" }));
    expect(await dialog.findByText(/Password too short/)).toBeInTheDocument();
    expect(dialog.getByLabelText(/^Password/)).toHaveAttribute("aria-invalid", "true");
    expect(dialog.getByLabelText(/^Username/)).not.toHaveAttribute("aria-invalid");
    expect(users).toHaveLength(before);
  });

  it("asks for a username and a password before asking the server", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Add a user" }));

    await user.click(dialog.getByRole("button", { name: "Create account" }));
    expect(await dialog.findByText("Choose a username.")).toBeInTheDocument();

    await user.type(dialog.getByLabelText(/^Username/), "erin");
    await user.click(dialog.getByRole("button", { name: "Create account" }));
    expect(await dialog.findByText("Set a password, or generate one.")).toBeInTheDocument();
    expect(users).toHaveLength(before);
  });

  it("says as you type whether the username is free or taken", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Add a user" }));
    await user.type(dialog.getByLabelText(/^Username/), "alice");
    expect(await dialog.findByText("@alice:example.org is taken.")).toBeInTheDocument();
    await user.clear(dialog.getByLabelText(/^Username/));
    await user.type(dialog.getByLabelText(/^Username/), "carol");
    expect(await dialog.findByText("@carol:example.org is free.")).toBeInTheDocument();
  });

  it("says as you type why a name can never be a username, in the server's words", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Add a user" }));
    await user.type(dialog.getByLabelText(/^Username/), "bad name");
    expect(
      await dialog.findByText(/^"bad name" cannot be a username: use lowercase letters/),
    ).toBeInTheDocument();
    await user.clear(dialog.getByLabelText(/^Username/));
    await user.type(dialog.getByLabelText(/^Username/), "carol:elsewhere.org");
    expect(
      await dialog.findByText("This server is example.org, not elsewhere.org."),
    ).toBeInTheDocument();
    // Case is not a different account: Alice is alice, and taken.
    await user.clear(dialog.getByLabelText(/^Username/));
    await user.type(dialog.getByLabelText(/^Username/), "Alice");
    expect(await dialog.findByText("@alice:example.org is taken.")).toBeInTheDocument();
  });

  it("records the kind of account it makes, explained", async () => {
    const user = userEvent.setup();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Add a user" }));
    const kind = dialog.getByRole("combobox", { name: /Kind of account/ });
    expect(kind).toHaveAccessibleDescription(/does not change what the account can do/);
    kind.focus();
    await user.keyboard("{Enter}");
    await user.click(await screen.findByRole("option", { name: "Support account" }));
    await user.type(dialog.getByLabelText(/^Username/), "helpdesk");
    await user.type(dialog.getByLabelText(/^Password/), "hunter2-helpdesk");
    await user.click(dialog.getByRole("button", { name: "Create account" }));
    await screen.findByRole("dialog", { name: "Account created" });
    expect(users.find((u) => u.user_id === "@helpdesk:example.org")?.user_type).toBe("support");
  });

  it("says when this server cannot check usernames in advance", async () => {
    server.use(
      http.get("*/api/v1/users/availability", () =>
        HttpResponse.json(
          {
            type: "urn:hs:problem:unavailable",
            title: "Unavailable",
            status: 503,
            detail: "this user directory does not support availability checks yet",
          },
          { status: 503 },
        ),
      ),
    );
    const user = userEvent.setup();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Add a user" }));
    await user.type(dialog.getByLabelText(/^Username/), "carol");
    expect(
      await dialog.findByText(/can’t check usernames in advance; a taken one is refused on Create/),
    ).toBeInTheDocument();
  });

  it("forgets what was typed when it closes", async () => {
    const user = userEvent.setup();
    const { onOpenChange } = renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Add a user" }));

    await user.type(dialog.getByLabelText(/^Password/), "hunter2-secret");
    await user.click(dialog.getByRole("button", { name: "Cancel" }));

    expect(onOpenChange).toHaveBeenCalledWith(false);
    expect((dialog.getByLabelText(/^Password/) as HTMLInputElement).value).toBe("");
  });
});
