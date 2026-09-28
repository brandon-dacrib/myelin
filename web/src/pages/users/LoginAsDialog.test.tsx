import { afterEach, describe, expect, it, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { useState } from "react";
import { server } from "@/mocks/node";
import { findUser } from "@/mocks/data/users";
import { listSessions } from "@/mocks/data/user-moderation";
import { signIn, signOut } from "@/lib/auth";
import { LoginAsDialog } from "./LoginAsDialog";

const ALICE = "@alice:example.org";

/** Opens the dialog from a button, as the card does, so it can be closed and opened again. */
function Harness({ onClose }: { onClose?: () => void }) {
  const [open, setOpen] = useState(true);
  return (
    <>
      <button onClick={() => setOpen(true)}>Open</button>
      <LoginAsDialog
        userId={ALICE}
        open={open}
        onOpenChange={(next) => {
          setOpen(next);
          if (!next) onClose?.();
        }}
      />
    </>
  );
}

function renderDialog(onClose?: () => void) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <Harness onClose={onClose} />
    </QueryClientProvider>,
  );
}

afterEach(() => {
  server.events.removeAllListeners();
  signOut();
});

describe("LoginAsDialog", () => {
  it("names the user, warns, and asks why before minting anything", async () => {
    await signIn();
    const user = userEvent.setup();
    renderDialog();
    const dialog = within(await screen.findByRole("dialog", { name: `Sign in as ${ALICE}?` }));
    expect(dialog.getByRole("note")).toHaveTextContent(/audit log/);
    await user.click(dialog.getByRole("button", { name: "Create support token" }));
    expect(await dialog.findByText(/Say why/)).toBeInTheDocument();
    expect(listSessions(ALICE).some((s) => s.support_session)).toBe(false);
  });

  it("shows the token once, with a copy button, and forgets it on close", async () => {
    await signIn();
    const user = userEvent.setup();
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
    let body: unknown = null;
    server.events.on("request:start", async ({ request }) => {
      if (request.url.endsWith("/login-as")) body = await request.clone().json();
    });
    const onClose = vi.fn();
    renderDialog(onClose);

    const ask = within(await screen.findByRole("dialog", { name: `Sign in as ${ALICE}?` }));
    await user.type(ask.getByLabelText(/^Reason/), "ticket 4412: cannot see room");
    await user.click(ask.getByRole("button", { name: "Create support token" }));

    const done = within(await screen.findByRole("dialog", { name: `Support token for ${ALICE}` }));
    expect(body).toMatchObject({ reason: "ticket 4412: cannot see room", valid_for_seconds: 3600 });
    const token = done.getByTestId("login-as-token").textContent!;
    expect(token).toMatch(/^mock_support_/);
    expect(done.getByText(/cannot be shown again/)).toBeInTheDocument();
    await user.click(done.getByRole("button", { name: "Copy access token" }));
    expect(writeText).toHaveBeenCalledWith(token);
    expect(listSessions(ALICE).filter((s) => s.support_session)).toHaveLength(1);

    await user.click(done.getByRole("button", { name: "Done" }));
    expect(onClose).toHaveBeenCalled();
    await user.click(screen.getByRole("button", { name: "Open" }));
    expect(await screen.findByRole("dialog", { name: `Sign in as ${ALICE}?` })).toBeInTheDocument();
    expect(screen.queryByText(token)).not.toBeInTheDocument();
  });

  it("says why when the account is deactivated", async () => {
    await signIn();
    findUser(ALICE)!.deactivated = true;
    const user = userEvent.setup();
    renderDialog();
    const ask = within(await screen.findByRole("dialog", { name: `Sign in as ${ALICE}?` }));
    await user.type(ask.getByLabelText(/^Reason/), "support");
    await user.click(ask.getByRole("button", { name: "Create support token" }));
    expect(await ask.findByText(/is deactivated/)).toBeInTheDocument();
  });
});
