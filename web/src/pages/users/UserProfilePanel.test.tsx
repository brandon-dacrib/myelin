import { afterEach, describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { server } from "@/mocks/node";
import { users } from "@/mocks/data/users";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { UserDetailPage } from "../UserDetailPage";
import { Toaster } from "@/components/ui/toast/Toaster";

/**
 * "How they appear" on a user's page: display name, avatar and kind of account edited in place,
 * against the MSW handlers, which answer as the real server does
 * (`crates/hs-cli/tests/admin_user_profile.rs` is the proof against the binary, including the
 * membership update in the user's rooms).
 */
const ROUTES = [{ path: "/users/$userId", component: UserDetailPage }];
const KNOWN = ["/", "/users", "/rooms/$roomId", "/audit", "/bridges/$bridgeId"];
const ALICE = "@alice:example.org";
const SLOW = { timeout: 5000 };

const alice = () => users.find((u) => u.user_id === ALICE)!;
const aliceBefore = { ...alice() };

afterEach(() => {
  Object.assign(alice(), aliceBefore);
  server.events.removeAllListeners();
  signOut();
});

function recordPatches(): Record<string, unknown>[] {
  const bodies: Record<string, unknown>[] = [];
  server.events.on("request:start", async ({ request }) => {
    if (
      request.method === "PATCH" &&
      new URL(request.url).pathname.endsWith("%40alice%3Aexample.org")
    )
      bodies.push((await request.clone().json()) as Record<string, unknown>);
  });
  return bodies;
}

async function openAlice() {
  renderRoutes(ROUTES, `/users/${encodeURIComponent(ALICE)}`, KNOWN);
  render(<Toaster />);
  const heading = await screen.findByRole("heading", { name: "How they appear" }, SLOW);
  return within(heading.closest("section")!);
}

describe("How they appear", () => {
  it(
    "says a change reaches every room they are in, and sends only what changed",
    { timeout: 20_000 },
    async () => {
      await signIn();
      const bodies = recordPatches();
      const user = userEvent.setup();
      const panel = await openAlice();
      expect(
        panel.getByText(/every room they are in gets an update to their membership/),
      ).toBeVisible();
      expect(panel.getByRole("button", { name: "Save profile" })).toBeDisabled();

      const name = panel.getByLabelText(/^Display name/);
      await user.clear(name);
      await user.type(name, "Alice Liddell");
      const kind = panel.getByRole("combobox", { name: /Kind of account/ });
      kind.focus();
      await user.keyboard("{Enter}");
      await user.click(await screen.findByRole("option", { name: "Bot" }));
      await user.click(panel.getByRole("button", { name: "Save profile" }));

      expect(
        await screen.findByRole("heading", { name: "Alice Liddell" }, SLOW),
      ).toBeInTheDocument();
      expect(bodies).toEqual([{ display_name: "Alice Liddell", user_type: "bot" }]);
      expect(alice()).toMatchObject({ display_name: "Alice Liddell", user_type: "bot" });
      expect(
        await screen.findByText(/Their rooms are being updated/, undefined, SLOW),
      ).toBeInTheDocument();
    },
  );

  it("clears a name with null, and an emptied avatar too", { timeout: 20_000 }, async () => {
    await signIn();
    alice().avatar_url = "mxc://example.org/alice";
    const bodies = recordPatches();
    const user = userEvent.setup();
    const panel = await openAlice();
    await user.clear(panel.getByLabelText(/^Display name/));
    await user.clear(panel.getByLabelText(/^Avatar URL/));
    await user.click(panel.getByRole("button", { name: "Save profile" }));
    expect(await screen.findByRole("heading", { name: ALICE }, SLOW)).toBeInTheDocument();
    expect(bodies).toEqual([{ display_name: null, avatar_url: null }]);
    expect(alice()).toMatchObject({ display_name: null, avatar_url: null });
  });

  it(
    "puts a refused avatar beside the field, in the server's words, and can undo",
    { timeout: 20_000 },
    async () => {
      await signIn();
      const user = userEvent.setup();
      const panel = await openAlice();
      const avatar = panel.getByLabelText(/^Avatar URL/);
      await user.type(avatar, "https://example.org/alice.png");
      await user.click(panel.getByRole("button", { name: "Save profile" }));
      expect(
        await panel.findByText("must be the mxc:// address of an uploaded image", undefined, SLOW),
      ).toBeInTheDocument();
      expect(avatar).toHaveAttribute("aria-invalid", "true");
      expect(alice().avatar_url).toBeNull();

      await user.click(panel.getByRole("button", { name: "Undo changes" }));
      expect(avatar).toHaveValue("");
      expect(avatar).not.toHaveAttribute("aria-invalid");
    },
  );

  it("is read-only without admin:write", { timeout: 20_000 }, async () => {
    await signIn(["admin:read"]);
    const panel = await openAlice();
    expect(panel.getByLabelText(/^Display name/)).toBeDisabled();
    expect(panel.getByRole("combobox", { name: /Kind of account/ })).toBeDisabled();
    expect(panel.queryByRole("button", { name: "Save profile" })).not.toBeInTheDocument();
  });
});
