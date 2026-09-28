import { afterEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { mintSupportSession } from "@/mocks/data/user-moderation";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { ActivityCard } from "./ActivityCard";

const ALICE = "@alice:example.org";
const SLOW = { timeout: 5000 };

/** Renders the card and waits for the router to mount it, which is slow on a loaded machine. */
async function renderCard(userId = ALICE) {
  renderRoutes([{ path: "/", component: () => <ActivityCard userId={userId} /> }], "/", [
    "/rooms/$roomId",
  ]);
  await screen.findByRole("tablist", { name: "Activity" }, SLOW);
}

afterEach(() => {
  server.events.removeAllListeners();
  signOut();
});

describe("ActivityCard", { timeout: 20_000 }, () => {
  it("lists sessions and marks a support session", async () => {
    await signIn();
    mintSupportSession(ALICE, 3600);
    await renderCard();
    const table = within(await screen.findByRole("table", { name: "Sessions" }, SLOW));
    expect(await table.findByText("ALICEWEB")).toBeInTheDocument();
    expect(table.getByText("198.51.100.4")).toBeInTheDocument();
    expect(table.getByText("Support session")).toBeInTheDocument();
  });

  it("lists rooms with links to each room's page, and filters by membership", async () => {
    await signIn();
    const user = userEvent.setup();
    let asked: string | null = "unset";
    server.events.on("request:start", ({ request }) => {
      if (request.url.includes("/memberships"))
        asked = new URL(request.url).searchParams.get("membership");
    });
    await renderCard();
    await user.click(screen.getByRole("tab", { name: "Rooms" }));
    const rooms = within(await screen.findByRole("table", { name: "Rooms" }, SLOW));
    const link = await rooms.findByRole("link", { name: /General/ });
    expect(link).toHaveAttribute("href", `/rooms/${encodeURIComponent("!general:example.org")}`);
    expect(rooms.getAllByText("Joined")).toHaveLength(2);
    expect(asked).toBeNull();

    await user.click(screen.getByRole("combobox", { name: "Membership" }));
    await user.click(await screen.findByRole("option", { name: "Banned" }));
    expect(await screen.findByText("No rooms with that membership.", {}, SLOW)).toBeVisible();
    expect(asked).toBe("ban");
  });

  it("shows statistics, with a dash for what the server could not count", async () => {
    await signIn();
    server.use(
      http.get("*/api/v1/users/:user_id/statistics", () =>
        HttpResponse.json({
          user_id: ALICE,
          joins_count: 2,
          invites_sent_count: 7,
          events_sent_count: 1204,
          rooms_created_count: 1,
          media_count: null,
          media_bytes: null,
          session_count: null,
        }),
      ),
    );
    const user = userEvent.setup();
    await renderCard();
    await user.click(screen.getByRole("tab", { name: "Statistics" }));
    expect(await screen.findByText("1,204", {}, SLOW)).toBeInTheDocument();
    expect(screen.getByText("Events sent")).toBeInTheDocument();
    expect(screen.getAllByText("—")).toHaveLength(3);
  });

  it("lists what they uploaded", async () => {
    await signIn();
    const user = userEvent.setup();
    await renderCard();
    await user.click(screen.getByRole("tab", { name: "Media" }));
    const media = within(await screen.findByRole("table", { name: "Media" }, SLOW));
    expect(await media.findByText("vacation.jpg")).toBeInTheDocument();
    expect(screen.getByText("1 file")).toBeInTheDocument();
  });

  it("says so when a tab cannot load", async () => {
    await signIn();
    server.use(
      http.get("*/api/v1/users/:user_id/sessions", () =>
        HttpResponse.json(
          { type: "urn:hs:problem:not-implemented", title: "Not implemented", status: 501 },
          { status: 501 },
        ),
      ),
    );
    await renderCard();
    const panel = screen.getByRole("tabpanel", { name: "Sessions" });
    await waitFor(
      () =>
        expect(within(panel).getByRole("status")).toHaveTextContent(
          "This user's sessions isn't implemented on this server yet",
        ),
      SLOW,
    );
  });
});
