import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { federationDestinations } from "@/mocks/data/dashboard";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { Toaster } from "@/components/ui/toast/Toaster";
import { FederationDestinationPage } from "../FederationDestinationPage";
import { FederationPage } from "../FederationPage";
import { validateFederationSearch } from "./federation-search";

/**
 * Forgetting a destination (decision 0042): from the list, filtered to the servers sharing no
 * room, and from the destination's own page; the warning and the force when a room is shared;
 * the server's own refusal when the list could not say.
 */
function open(path: string) {
  return renderRoutes(
    [
      {
        path: "/federation",
        component: () => (
          <>
            <FederationPage />
            <Toaster />
          </>
        ),
        validateSearch: validateFederationSearch,
      },
      {
        path: "/federation/$serverName",
        component: () => (
          <>
            <FederationDestinationPage />
            <Toaster />
          </>
        ),
      },
    ],
    path,
    ["/configuration/$section", "/rooms/$roomId", "/tasks/$taskId"],
  );
}

async function serverNames(): Promise<string[]> {
  const table = await screen.findByRole("table", { name: "Federation destinations" });
  return within(table)
    .getAllByRole("row")
    .slice(1)
    .map((row) => within(row).getAllByRole("link")[0].textContent ?? "");
}

/** The table's row for a server (the card fallback below 768px has a link of the same name). */
function rowFor(name: string): HTMLElement {
  const table = screen.getByRole("table", { name: "Federation destinations" });
  return within(table).getByRole("link", { name }).closest("tr")!;
}

describe("Forgetting a destination", () => {
  beforeEach(async () => {
    await signIn();
  });
  afterEach(() => {
    server.resetHandlers();
    signOut();
  });

  it("lists the servers sharing no room, says how many, and forgets one from its row", async () => {
    const { router } = open("/federation");
    await serverNames();
    await userEvent.click(screen.getByRole("button", { name: "No shared room" }));
    await waitFor(() => expect(router.state.location.search).toEqual({ show: "no-shared-room" }));
    await waitFor(async () => expect(await serverNames()).toHaveLength(8));
    expect(screen.getByText("8 servers sharing no room")).toBeInTheDocument();
    expect(await serverNames()).toContain("srv-07.example.net");
    const row = rowFor("srv-07.example.net");
    expect(within(row).getByText("none")).toBeInTheDocument();

    await userEvent.click(within(row).getByRole("button", { name: "Forget srv-07.example.net" }));
    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByText(/No room is shared with it/)).toBeInTheDocument();
    expect(dialog.getByText(/its retry state and its cached signing keys/)).toBeInTheDocument();
    expect(dialog.queryByRole("switch")).not.toBeInTheDocument();
    await userEvent.click(dialog.getByRole("button", { name: "Forget" }));

    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(await screen.findByText("Forgot srv-07.example.net")).toBeInTheDocument();
    expect(screen.getByText("Nothing was queued for it.")).toBeInTheDocument();
    await waitFor(async () => expect(await serverNames()).not.toContain("srv-07.example.net"));
    expect(screen.getByText("7 servers sharing no room")).toBeInTheDocument();
    expect(federationDestinations.some((d) => d.server_name === "srv-07.example.net")).toBe(false);
  });

  it("warns when a room is shared and forgets only once the operator insists", async () => {
    open("/federation");
    await serverNames();
    const row = rowFor("mozilla.org");
    expect(within(row).getByText("2")).toBeInTheDocument();
    await userEvent.click(within(row).getByRole("button", { name: "Forget mozilla.org" }));

    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByText(/42 queued events \(unsent, lost\)/)).toBeInTheDocument();
    expect(dialog.getByText(/still shares 2 rooms with it/)).toBeInTheDocument();
    const forget = dialog.getByRole("button", { name: "Forget" });
    expect(forget).toBeDisabled();
    await userEvent.click(dialog.getByRole("switch", { name: "Forget it anyway" }));
    expect(forget).toBeEnabled();
    await userEvent.click(forget);

    expect(await screen.findByText("Forgot mozilla.org")).toBeInTheDocument();
    expect(screen.getByText("Dropped 42 events, 3 messages.")).toBeInTheDocument();
    await waitFor(async () => expect(await serverNames()).not.toContain("mozilla.org"));
  });

  it("shows the server's refusal when it could not say rooms were shared, then offers the force", async () => {
    const deletes: string[] = [];
    server.use(
      http.get("/api/v1/federation/destinations", () =>
        HttpResponse.json({
          items: federationDestinations
            .slice(0, 2)
            .map((d) => ({ ...d, shared_rooms_count: null })),
          next_cursor: null,
          prev_cursor: null,
          total: 2,
        }),
      ),
      http.delete("/api/v1/federation/destinations/:server_name", ({ request }) => {
        deletes.push(new URL(request.url).search);
        if (new URL(request.url).searchParams.get("force") !== "true")
          return HttpResponse.json(
            {
              type: "urn:hs:problem:conflict",
              title: "Conflict",
              status: 409,
              detail:
                "this server still shares 1 room with matrix.org; Forget it anyway with force=true",
            },
            { status: 409 },
          );
        return HttpResponse.json({
          server_name: "matrix.org",
          dropped_pdu_count: 0,
          dropped_edu_count: 0,
          dropped_key_count: 1,
          was_catching_up: false,
          shared_rooms_count: 1,
        });
      }),
    );
    open("/federation");
    await serverNames();
    const row = rowFor("matrix.org");
    expect(within(row).getByText("unknown")).toBeInTheDocument();
    await userEvent.click(within(row).getByRole("button", { name: "Forget matrix.org" }));

    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByText(/cannot say whether it shares a room/)).toBeInTheDocument();
    await userEvent.click(dialog.getByRole("button", { name: "Forget" }));
    expect(await dialog.findByRole("alert")).toHaveTextContent(
      /Couldn't forget matrix\.org\. this server still shares 1 room/,
    );
    expect(
      dialog.getByText(/The server refused because it still shares a room/),
    ).toBeInTheDocument();
    expect(dialog.getByRole("button", { name: "Forget" })).toBeDisabled();
    await userEvent.click(dialog.getByRole("switch", { name: "Forget it anyway" }));
    await userEvent.click(dialog.getByRole("button", { name: "Forget" }));

    expect(await screen.findByText("Forgot matrix.org")).toBeInTheDocument();
    expect(screen.getByText("Dropped 1 signing key.")).toBeInTheDocument();
    expect(deletes).toEqual(["", "?force=true"]);
  });

  it("forgets from the destination's page and returns to the list", async () => {
    const { router } = open("/federation/srv-14.example.net");
    expect(
      await screen.findByText(
        "None: nothing will be sent to it until a room brings the two together",
      ),
    ).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Forget this server" }));
    const dialog = within(await screen.findByRole("dialog"));
    await userEvent.click(dialog.getByRole("button", { name: "Forget" }));
    await waitFor(() => expect(router.state.location.pathname).toBe("/federation"));
    expect(await screen.findByText("Forgot srv-14.example.net")).toBeInTheDocument();
  });

  it("is not offered without admin:write", async () => {
    signOut();
    await signIn(["admin:read"]);
    open("/federation");
    await serverNames();
    const row = rowFor("srv-07.example.net");
    expect(within(row).getByRole("button", { name: "Forget srv-07.example.net" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Preview prune" })).toBeDisabled();
  });
});
