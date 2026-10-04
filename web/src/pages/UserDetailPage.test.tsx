import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { server } from "@/mocks/node";
import { findUser } from "@/mocks/data/users";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { UserDetailPage } from "./UserDetailPage";

const KNOWN = ["/", "/rooms/$roomId", "/audit", "/bridges/$bridgeId", "/users"];

function open(userId: string) {
  return renderRoutes(
    [{ path: "/users/$userId", component: UserDetailPage }],
    `/users/${encodeURIComponent(userId)}`,
    KNOWN,
  );
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => {
  server.events.removeAllListeners();
  signOut();
});

/** The JSON bodies posted to `/users/{id}/deactivate` while a test runs. */
function recordDeactivations(): unknown[] {
  const bodies: unknown[] = [];
  server.events.on("request:start", async ({ request }) => {
    if (request.method === "POST" && new URL(request.url).pathname.endsWith("/deactivate")) {
      bodies.push(await request.clone().json());
    }
  });
  return bodies;
}

describe("A user's page", () => {
  it("names the kind of account and the bridge that made it, in words", async () => {
    open("@whatsapp_15551234:example.org");
    expect(await screen.findByText("Kind of account", { selector: "dt" })).toBeInTheDocument();
    expect(screen.getByText("Made by a bridge")).toBeInTheDocument();
  });

  it("deactivates an account, says what that does, and reactivates it", async () => {
    const user = userEvent.setup();
    open("@spammer42:example.org");
    const danger = await screen.findByText("Deactivate this user");
    expect(danger.parentElement).toHaveTextContent(/signed out everywhere/);

    await user.click(screen.getByRole("button", { name: "Deactivate" }));
    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByText(/Reactivate on this page lets them sign in again/)).toBeVisible();
    await user.click(dialog.getByRole("button", { name: "Deactivate" }));

    const reactivate = await screen.findByRole("button", { name: "Reactivate" });
    expect(screen.getByText(/Rooms they were taken out of/)).toBeInTheDocument();
    await user.click(reactivate);
    expect(await screen.findByRole("button", { name: "Deactivate" })).toBeVisible();
    expect(screen.queryByRole("button", { name: "Reactivate" })).not.toBeInTheDocument();
  });

  it("deactivates and erases in one step when the box is ticked, and says what goes", async () => {
    const user = userEvent.setup();
    const posted = recordDeactivations();
    open("@bot:example.org");
    await user.click(await screen.findByRole("button", { name: "Deactivate" }));
    const dialog = within(await screen.findByRole("dialog"));

    // Off by default: the plain deactivation is the reversible one.
    const erase = dialog.getByRole("checkbox", { name: /Also erase their data/ });
    expect(erase).not.toBeChecked();
    expect(dialog.queryByText(/every device, with its encryption keys/)).not.toBeInTheDocument();
    await user.click(erase);
    expect(dialog.getByText(/every device, with its encryption keys/)).toBeVisible();
    expect(dialog.getByText(/messages they sent/)).toBeVisible();
    expect(
      dialog.getByText(/cannot be reactivated and the erasure cannot be undone/),
    ).toBeVisible();
    await user.click(dialog.getByRole("button", { name: "Deactivate and erase" }));

    expect(await screen.findByText("Erased")).toBeInTheDocument();
    expect(posted).toEqual([{ erase: true }]);
    expect(screen.getByText("Deactivated")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Reactivate" })).not.toBeInTheDocument();
    expect(screen.getByText("This account was erased")).toBeInTheDocument();
    expect(screen.getByRole("heading", { level: 1 })).toHaveTextContent("@bot:example.org");
    expect(screen.getByText("Cleared when the account was erased")).toBeInTheDocument();
  });

  it("sends a plain deactivation when the box is left alone", async () => {
    const user = userEvent.setup();
    const posted = recordDeactivations();
    open("@whatsapp_15551234:example.org");
    await user.click(await screen.findByRole("button", { name: "Deactivate" }));
    await user.click(
      within(await screen.findByRole("dialog")).getByRole("button", { name: "Deactivate" }),
    );
    await screen.findByRole("button", { name: "Reactivate" });
    expect(posted).toEqual([{}]);
    expect(screen.queryByText("Erased")).not.toBeInTheDocument();
  });

  it("offers to erase an account that is already deactivated", async () => {
    const user = userEvent.setup();
    const posted = recordDeactivations();
    findUser("@visitor7:example.org")!.deactivated = true;
    open("@visitor7:example.org");
    await screen.findByRole("button", { name: "Reactivate" });
    const box = screen.getByText("Erase this user's data").parentElement!;
    expect(box).toHaveTextContent(/can never be reactivated/);
    await user.click(within(box).getByRole("button", { name: "Erase data" }));
    const dialog = within(await screen.findByRole("dialog", { name: /Erase .* data\?/ }));
    expect(dialog.getByText(/single-sign-on links/)).toBeVisible();
    await user.click(dialog.getByRole("button", { name: "Erase data" }));

    expect(await screen.findByText("Erased")).toBeInTheDocument();
    expect(posted).toEqual([{ erase: true }]);
    expect(screen.queryByRole("button", { name: "Reactivate" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Erase data" })).not.toBeInTheDocument();
  });

  it("shows an erased account as one with nothing left and nothing to reactivate", async () => {
    open("@gone:example.org");
    expect(await screen.findByText("Erased")).toBeInTheDocument();
    expect(screen.getByText("Deactivated")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Reactivate" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Deactivate" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Erase data" })).not.toBeInTheDocument();
    expect(screen.getByText("This account was erased").parentElement).toHaveTextContent(
      /cannot be reactivated/,
    );
    expect(screen.getByText("Cleared when the account was erased")).toBeInTheDocument();
    expect(await screen.findByText("No devices.")).toBeInTheDocument();
    const reset = screen.getByRole("button", { name: "Reset password" });
    expect(reset).toBeDisabled();
    expect(reset).toHaveAttribute("title", "An erased account has no password to reset");
  });
});
