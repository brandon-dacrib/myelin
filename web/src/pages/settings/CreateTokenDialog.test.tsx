import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http } from "msw";
import { server } from "@/mocks/node";
import { CreateTokenDialog } from "./CreateTokenDialog";
import { signIn, signOut } from "@/lib/auth";

/** Every body POSTed to `/registration-tokens`, read on the way past to the mock's handler. */
function captureCreates(): unknown[] {
  const bodies: unknown[] = [];
  server.use(
    http.post("*/api/v1/registration-tokens", async ({ request }) => {
      bodies.push(await request.clone().json());
      // Returning nothing lets the request carry on to the mock's own handler.
    }),
  );
  return bodies;
}

function renderDialog() {
  const onOpenChange = vi.fn();
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <CreateTokenDialog open onOpenChange={onOpenChange} />
    </QueryClientProvider>,
  );
  return { onOpenChange };
}

const DAY = 86_400_000;

beforeEach(async () => {
  await signIn();
});

afterEach(() => {
  signOut();
});

describe("CreateTokenDialog", () => {
  it("generates a one-use token that expires in a week by default, and shows its invite link", async () => {
    const user = userEvent.setup();
    const bodies = captureCreates();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Create an invite link" }));

    expect(dialog.getByRole("radio", { name: "Generate one" })).toBeChecked();
    expect(dialog.getByLabelText(/^Length/)).toHaveValue(16);
    expect(dialog.getByLabelText(/^Uses allowed/)).toHaveValue(1);
    expect(dialog.getByRole("radio", { name: "7 days" })).toBeChecked();

    const before = Date.now();
    await user.click(dialog.getByRole("button", { name: "Create invite link" }));

    const done = within(await screen.findByRole("dialog", { name: "Invite link ready" }));
    const body = bodies[0] as {
      token?: string;
      length: number;
      uses_allowed: number | null;
      expires_at: string;
    };
    expect(body.token).toBeUndefined();
    expect(body.length).toBe(16);
    expect(body.uses_allowed).toBe(1);
    const expires = Date.parse(body.expires_at);
    expect(expires).toBeGreaterThanOrEqual(before + 7 * DAY - 1000);
    expect(expires).toBeLessThanOrEqual(Date.now() + 7 * DAY + 1000);

    // The link is shown in full, and one click copies it.
    const link = done.getByText(/\/admin\/register\?token=/);
    expect(link.textContent).toMatch(
      new RegExp(`^${window.location.origin}/admin/register\\?token=[A-Za-z0-9]{16}$`),
    );
    await user.click(done.getByRole("button", { name: "Copy invite link" }));
    await expect(navigator.clipboard.readText()).resolves.toBe(link.textContent);
    expect(done.getByRole("button", { name: "Copied" })).toBeInTheDocument();
  });

  it("uses a chosen token, unlimited uses and no expiry when asked", async () => {
    const user = userEvent.setup();
    const bodies = captureCreates();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Create an invite link" }));

    await user.click(dialog.getByRole("radio", { name: "Choose my own" }));
    expect(dialog.queryByLabelText(/^Length/)).not.toBeInTheDocument();
    await user.type(dialog.getByLabelText(/^Custom token/), "spring-2026");
    await user.click(dialog.getByRole("switch", { name: "Unlimited" }));
    expect(dialog.getByLabelText(/^Uses allowed/)).toBeDisabled();
    await user.click(dialog.getByRole("radio", { name: "Never" }));
    await user.click(dialog.getByRole("button", { name: "Create invite link" }));

    const done = within(await screen.findByRole("dialog", { name: "Invite link ready" }));
    expect(bodies[0]).toMatchObject({ token: "spring-2026", uses_allowed: null, expires_at: null });
    expect(
      done.getByText(`${window.location.origin}/admin/register?token=spring-2026`),
    ).toBeInTheDocument();
    expect(done.getByText("Unlimited")).toBeInTheDocument();
    expect(done.getByText("Never")).toBeInTheDocument();
  });

  it("takes a date and time for the expiry", async () => {
    const user = userEvent.setup();
    const bodies = captureCreates();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Create an invite link" }));

    await user.click(dialog.getByRole("radio", { name: "Date and time" }));
    await user.click(dialog.getByRole("button", { name: "Create invite link" }));
    expect(await dialog.findByText("Choose the date and time it expires.")).toBeInTheDocument();

    const when = new Date(Date.now() + 3 * DAY);
    const pad = (n: number) => String(n).padStart(2, "0");
    const local = `${when.getFullYear()}-${pad(when.getMonth() + 1)}-${pad(when.getDate())}T09:30`;
    await user.type(dialog.getByLabelText(/^Expires at/), local);
    await user.clear(dialog.getByLabelText(/^Uses allowed/));
    await user.type(dialog.getByLabelText(/^Uses allowed/), "5");
    await user.click(dialog.getByRole("button", { name: "Create invite link" }));

    await screen.findByRole("dialog", { name: "Invite link ready" });
    expect(bodies[0]).toMatchObject({
      uses_allowed: 5,
      expires_at: new Date(local).toISOString(),
    });
  });

  it("checks a chosen token before sending it, and puts a taken one beside the field", async () => {
    const user = userEvent.setup();
    const bodies = captureCreates();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Create an invite link" }));

    await user.click(dialog.getByRole("radio", { name: "Choose my own" }));
    await user.type(dialog.getByLabelText(/^Custom token/), "not allowed!");
    await user.click(dialog.getByRole("button", { name: "Create invite link" }));
    expect(await dialog.findByText(/Only letters, digits/)).toBeInTheDocument();
    expect(dialog.getByLabelText(/^Custom token/)).toHaveAttribute("aria-invalid", "true");
    expect(bodies).toHaveLength(0);

    await user.clear(dialog.getByLabelText(/^Custom token/));
    await user.type(dialog.getByLabelText(/^Custom token/), "welcome-team");
    await user.click(dialog.getByRole("button", { name: "Create invite link" }));
    expect(
      await dialog.findByText('a registration token "welcome-team" already exists'),
    ).toBeInTheDocument();
    expect(dialog.getByLabelText(/^Custom token/)).toHaveAttribute("aria-invalid", "true");
  });

  it("asks for a whole number of uses", async () => {
    const user = userEvent.setup();
    const bodies = captureCreates();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: "Create an invite link" }));

    await user.clear(dialog.getByLabelText(/^Uses allowed/));
    await user.click(dialog.getByRole("button", { name: "Create invite link" }));
    expect(await dialog.findByText("A whole number, or turn on Unlimited.")).toBeInTheDocument();
    expect(bodies).toHaveLength(0);
  });
});
