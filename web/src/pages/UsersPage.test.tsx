import { afterEach, describe, expect, it } from "vitest";
import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { UsersPage } from "./UsersPage";
import { UserDetailPage } from "./UserDetailPage";

/**
 * The Users page's own ways into Settings' flows: "Invite by link" on the list opens the same
 * registration-token dialog as Settings, Registration tokens, and "Send notice" on a user's page
 * opens the server-notice form with that user as the one recipient. The dialogs themselves are
 * tested beside them (`settings/CreateTokenDialog.test.tsx`, `settings/ServerNoticesPage.test.tsx`);
 * this is about where they are reachable from, and who may reach them.
 */
const ROUTES = [
  {
    path: "/users",
    component: UsersPage,
    validateSearch: (s: Record<string, unknown>) => ({
      q: typeof s.q === "string" ? s.q : undefined,
      cursor: typeof s.cursor === "string" ? s.cursor : undefined,
    }),
  },
  { path: "/users/$userId", component: UserDetailPage },
];
const KNOWN = ["/", "/rooms/$roomId", "/settings/registration-tokens", "/audit"];

afterEach(() => {
  server.events.removeAllListeners();
  signOut();
});

describe("Users page: invite by link", () => {
  it("makes an invite link from the list, with Settings' dialog", async () => {
    await signIn();
    const user = userEvent.setup();
    let posted: unknown = null;
    server.events.on("request:start", async ({ request }) => {
      if (
        request.method === "POST" &&
        new URL(request.url).pathname.endsWith("/registration-tokens")
      ) {
        posted = await request.clone().json();
      }
    });
    renderRoutes(ROUTES, "/users", KNOWN);

    await user.click(await screen.findByRole("button", { name: "Invite by link" }));
    const dialog = within(await screen.findByRole("dialog", { name: "Create an invite link" }));
    // Starts where inviting one person starts: one use, a week.
    expect(dialog.getByLabelText(/^Uses allowed/)).toHaveValue(1);
    await user.click(dialog.getByRole("button", { name: "Create invite link" }));

    const done = within(await screen.findByRole("dialog", { name: "Invite link ready" }));
    expect(done.getByText(/\/admin\/register\?token=/)).toBeInTheDocument();
    expect(posted).toMatchObject({ uses_allowed: 1 });
  });

  it("is not offered to somebody who can only read", async () => {
    await signIn(["admin:read"]);
    renderRoutes(ROUTES, "/users", KNOWN);
    await screen.findByRole("table");
    expect(screen.queryByRole("button", { name: "Invite by link" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Add user" })).not.toBeInTheDocument();
  });
});

describe("A user's page: send notice", () => {
  it("sends a notice to that user alone", async () => {
    await signIn();
    const user = userEvent.setup();
    let posted: unknown = null;
    server.use(
      http.post("*/api/v1/server-notices", async ({ request }) => {
        posted = await request.json();
        return HttpResponse.json(
          {
            id: "01NOTICE",
            sender: "@_server:example.org",
            type: "m.room.message",
            content: { msgtype: "m.text", body: "Please check your email." },
            recipients: ["@alice:example.org"],
            room_ids: ["!notices-alice:example.org"],
            event_ids: ["$notice"],
            sent_at: new Date().toISOString(),
          },
          { status: 201 },
        );
      }),
    );
    renderRoutes(ROUTES, "/users/%40alice%3Aexample.org", KNOWN);

    await user.click(await screen.findByRole("button", { name: "Send notice" }));
    const dialog = within(await screen.findByRole("dialog", { name: "Send a server notice" }));
    expect(dialog.getByText("@alice:example.org")).toBeInTheDocument();
    await user.type(dialog.getByLabelText(/^Message/), "Please check your email.");
    await user.click(dialog.getByRole("button", { name: "Send notice" }));

    expect(await dialog.findByText("Notice sent to 1 user.")).toBeInTheDocument();
    expect(posted).toMatchObject({
      recipients: ["@alice:example.org"],
      content: { msgtype: "m.text", body: "Please check your email." },
    });
  });

  it("says what is missing to somebody who cannot moderate", async () => {
    await signIn(["admin:read"]);
    renderRoutes(ROUTES, "/users/%40alice%3Aexample.org", KNOWN);
    const send = await screen.findByRole("button", { name: "Send notice" });
    expect(send).toBeDisabled();
    expect(send).toHaveAttribute("title", "Needs moderation:write");
  });
});

describe("Users page: guests", () => {
  it("marks a guest account with a badge that says what a guest is", async () => {
    await signIn();
    renderRoutes(ROUTES, "/users", KNOWN);
    const rowOf = (userId: string) =>
      screen
        .getAllByText(userId)
        .map((cell) => cell.closest("tr"))
        .find((row): row is HTMLTableRowElement => row !== null) as HTMLElement;
    await screen.findAllByText("@visitor7:example.org");
    const badge = within(rowOf("@visitor7:example.org")).getByText("Guest");
    expect(badge.closest("[title]")?.getAttribute("title")).toMatch(/no password/);
    expect(within(rowOf("@alice:example.org")).queryByText("Guest")).not.toBeInTheDocument();
  });
});
