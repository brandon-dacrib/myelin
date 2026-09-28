import { afterEach, describe, expect, it } from "vitest";
import { screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { RoomsPage } from "./RoomsPage";
import { validateRoomSearch } from "./rooms/room-search";

function searchAndCursor(search: Record<string, unknown>) {
  return {
    q: typeof search.q === "string" ? search.q : undefined,
    cursor: typeof search.cursor === "string" ? search.cursor : undefined,
  };
}

const ROUTES = [
  { path: "/rooms", component: RoomsPage, validateSearch: searchAndCursor },
  { path: "/rooms/$roomId", validateSearch: validateRoomSearch },
];

afterEach(() => signOut());

describe("RoomsPage", () => {
  it("finds an event by its id and opens it in its room's timeline", async () => {
    await signIn();
    const { router } = renderRoutes(ROUTES, "/rooms");
    await userEvent.type(await screen.findByLabelText("Find an event by its ID"), "$msg-gener-4");
    await userEvent.click(screen.getByRole("button", { name: "Find event" }));
    await waitFor(() =>
      expect(router.state.location.pathname).toBe("/rooms/!general%3Aexample.org"),
    );
    expect(router.state.location.search).toEqual({ tab: "timeline", event: "$msg-gener-4" });
  });

  it("says so when no event has that id", async () => {
    await signIn();
    renderRoutes(ROUTES, "/rooms");
    await userEvent.type(await screen.findByLabelText("Find an event by its ID"), "$nope");
    await userEvent.click(screen.getByRole("button", { name: "Find event" }));
    expect(await screen.findByText(/Couldn.t find that event/)).toBeInTheDocument();
  });

  it("lists rooms for a moderator, without the event finder", async () => {
    await signIn(["moderation:read"]);
    renderRoutes(ROUTES, "/rooms");
    expect(await screen.findByRole("link", { name: "General" })).toBeInTheDocument();
    expect(screen.queryByLabelText("Find an event by its ID")).toBeNull();
  });
});
