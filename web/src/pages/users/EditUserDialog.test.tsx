import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { EditUserDialog } from "./EditUserDialog";
import { users } from "@/mocks/data/users";
import type { User } from "@/api/users";
import { signIn, signOut } from "@/lib/auth";

const ALICE = "@alice:example.org";

function renderDialog(user: User) {
  const onOpenChange = vi.fn();
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <EditUserDialog user={user} open onOpenChange={onOpenChange} />
    </QueryClientProvider>,
  );
  return { onOpenChange };
}

const alice = () => users.find((u) => u.user_id === ALICE)!;
const aliceBefore = { ...alice() };

beforeEach(async () => {
  await signIn();
});

afterEach(() => {
  Object.assign(alice(), aliceBefore);
  signOut();
});

describe("EditUserDialog", () => {
  it("grants server administrator and sends only that", async () => {
    const user = userEvent.setup();
    const bodies: unknown[] = [];
    server.use(
      http.patch("*/api/v1/users/:user_id", async ({ request }) => {
        const body = (await request.json()) as Record<string, unknown>;
        bodies.push(body);
        return HttpResponse.json({ ...alice(), ...body });
      }),
    );
    const { onOpenChange } = renderDialog(alice());
    const dialog = within(await screen.findByRole("dialog", { name: "Edit account" }));
    expect(dialog.getByText(/Only what you change is sent/)).toBeInTheDocument();
    await user.click(dialog.getByRole("switch", { name: /server administrator/i }));
    await user.click(dialog.getByRole("button", { name: "Save" }));
    await vi.waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
    expect(bodies).toEqual([{ admin: true }]);
  });

  it("changes the display name and avatar through the mock server", async () => {
    const user = userEvent.setup();
    const { onOpenChange } = renderDialog(alice());
    const dialog = within(await screen.findByRole("dialog", { name: "Edit account" }));
    const name = dialog.getByLabelText(/display name/i);
    await user.clear(name);
    await user.type(name, "Alice Liddell");
    await user.type(dialog.getByLabelText(/avatar/i), "mxc://example.org/alice");
    await user.click(dialog.getByRole("button", { name: "Save" }));
    await vi.waitFor(() => expect(onOpenChange).toHaveBeenCalledWith(false));
    expect(alice().display_name).toBe("Alice Liddell");
    expect(alice().avatar_url).toBe("mxc://example.org/alice");
    expect(alice().admin).toBe(false);
  });

  it("shows a field the server cannot change yet beside that field, in its words", async () => {
    const user = userEvent.setup();
    server.use(
      http.patch("*/api/v1/users/:user_id", () =>
        HttpResponse.json(
          {
            type: "urn:hs:problem:validation-failed",
            title: "Validation failed",
            status: 400,
            detail: "one or more fields in the request cannot be applied",
            errors: [
              { pointer: "/display_name", detail: "no data source can change this field yet" },
            ],
          },
          { status: 400 },
        ),
      ),
    );
    const { onOpenChange } = renderDialog(alice());
    const dialog = within(await screen.findByRole("dialog", { name: "Edit account" }));
    await user.type(dialog.getByLabelText(/display name/i), " L");
    await user.click(dialog.getByRole("button", { name: "Save" }));
    const alert = await dialog.findByRole("alert");
    expect(alert).toHaveTextContent("This server says: no data source can change this field yet.");
    expect(dialog.getByLabelText(/display name/i)).toHaveAttribute("aria-invalid", "true");
    expect(onOpenChange).not.toHaveBeenCalled();
  });

  it("warns when taking administrator away from the signed-in account", async () => {
    const user = userEvent.setup();
    renderDialog({ ...alice(), user_id: "@ops:example.org", admin: true });
    const dialog = within(await screen.findByRole("dialog", { name: "Edit account" }));
    expect(dialog.queryByText(/This is your own account/)).not.toBeInTheDocument();
    await user.click(dialog.getByRole("switch", { name: /server administrator/i }));
    expect(dialog.getByText(/This is your own account/)).toBeVisible();
  });

  it("closes without a request when nothing changed", async () => {
    const user = userEvent.setup();
    const calls: number[] = [];
    server.use(
      http.patch("*/api/v1/users/:user_id", () => {
        calls.push(1);
        return HttpResponse.json(alice());
      }),
    );
    const { onOpenChange } = renderDialog(alice());
    const dialog = within(await screen.findByRole("dialog", { name: "Edit account" }));
    await user.click(dialog.getByRole("button", { name: "Save" }));
    expect(onOpenChange).toHaveBeenCalledWith(false);
    expect(calls).toEqual([]);
  });
});
