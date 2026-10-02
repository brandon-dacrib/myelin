import { afterEach, describe, expect, it } from "vitest";
import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { signIn, signOut, type Scope } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { RoomDetailPage } from "./RoomDetailPage";
import { validateRoomSearch } from "./rooms/room-search";

const ROUTES = [
  { path: "/rooms/$roomId", component: RoomDetailPage, validateSearch: validateRoomSearch },
];
const KNOWN = ["/rooms", "/tasks/$taskId"];
const GENERAL = "/rooms/!general:example.org";
const TASK_WAIT = { timeout: 15_000 };

async function open(path = GENERAL, scopes?: Scope[]) {
  await signIn(scopes);
  const rendered = renderRoutes(ROUTES, path, KNOWN);
  await screen.findByRole("heading", { level: 1 });
  return rendered;
}

async function tab(name: string) {
  await userEvent.click(screen.getByRole("tab", { name }));
}

afterEach(() => signOut());

describe("Room lifecycle", () => {
  it("asks why on Block, shows the reason on the badge and in the facts, and clears it on Unblock", async () => {
    await open("/rooms/!spam-central:example.org");
    expect(screen.getByText("Guests may join")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Block" }));
    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByText(/Nobody can join it any more/)).toBeInTheDocument();
    await userEvent.type(dialog.getByLabelText(/^Reason/), "Spam ring");
    await userEvent.click(dialog.getByRole("button", { name: "Block" }));
    expect(await screen.findByText("Blocked: Spam ring")).toBeInTheDocument();
    expect(screen.getByText("Blocked because")).toBeInTheDocument();
    expect(screen.getByText("Spam ring")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Unblock" }));
    await waitFor(() => expect(screen.queryByText("Blocked: Spam ring")).toBeNull());
  });

  it("says an upgraded room is closed and links its successor", async () => {
    const { router } = await open("/rooms/!general-v6:example.org");
    expect(screen.getByText("Upgraded")).toBeInTheDocument();
    expect(screen.getByText(/This room was upgraded and closed/)).toBeInTheDocument();
    expect(screen.getByText("Upgraded to")).toBeInTheDocument();
    const links = screen.getAllByRole("link", { name: "!general:example.org" });
    expect(links.length).toBeGreaterThan(0);
    await userEvent.click(links[0]!);
    await waitFor(() =>
      expect(decodeURIComponent(router.state.location.pathname)).toBe(
        "/rooms/!general:example.org",
      ),
    );
    expect(await screen.findByRole("heading", { name: "General" })).toBeInTheDocument();
    expect(screen.queryByText("Upgraded")).toBeNull();
  });
});

describe("RoomDetailPage", () => {
  it("shows the overview with members, and a Space tab only for a space", async () => {
    await open();
    expect(screen.getByRole("heading", { name: "General" })).toBeInTheDocument();
    expect(screen.getByRole("tab", { name: "Overview" })).toHaveAttribute("aria-selected", "true");
    expect(await screen.findByText("@alice:example.org")).toBeInTheDocument();
    expect(screen.queryByRole("tab", { name: "Space" })).toBeNull();
  });

  it("lists a space's rooms, linking the ones this server holds", async () => {
    await open("/rooms/!engineering:example.org");
    await tab("Space");
    const list = await screen.findByRole("list", { name: "Hierarchy" });
    expect(within(list).getByRole("link", { name: "General" })).toBeInTheDocument();
    expect(within(list).getByText("!lobby:remote.example")).toBeInTheDocument();
    expect(within(list).getByText("Not on this server")).toBeInTheDocument();
  });

  it("lists the room's state and filters it by type", async () => {
    await open();
    await tab("State");
    const table = await screen.findByRole("table", { name: "Room state" });
    expect(within(table).getByText("m.room.create")).toBeInTheDocument();
    await userEvent.type(screen.getByLabelText("Filter state by event type"), "member");
    await waitFor(() => expect(within(table).queryByText("m.room.create")).toBeNull());
    expect(within(table).getAllByText("m.room.member").length).toBeGreaterThan(0);
  });

  it("reads the timeline newest first, loads older, and shows an event in context", async () => {
    await open();
    await tab("Timeline");
    expect(screen.getByText(/recorded in the audit log/)).toBeInTheDocument();
    const messages = await screen.findByRole("list", { name: "Messages" });
    expect(within(messages).getByText("Message 30 in General")).toBeInTheDocument();
    expect(within(messages).queryByText("Message 1 in General")).toBeNull();
    await userEvent.click(screen.getByRole("button", { name: "Load older" }));
    expect(await within(messages).findByText("Message 1 in General")).toBeInTheDocument();

    await userEvent.click(within(messages).getByText("Message 30 in General"));
    const panel = await screen.findByRole("complementary", { name: "Event in context" });
    expect(await within(panel).findByText("Message 29 in General")).toBeInTheDocument();
  });

  it("jumps to the event nearest a date", async () => {
    await open();
    await tab("Timeline");
    await screen.findByRole("list", { name: "Messages" });
    fireEvent.change(screen.getByLabelText("Jump to date"), {
      target: { value: "1970-01-02T00:00" },
    });
    await userEvent.click(screen.getByRole("button", { name: "Jump" }));
    const panel = await screen.findByRole("complementary", { name: "Event in context" });
    expect(await within(panel).findByText(/"room_version": "11"/)).toBeInTheDocument();
  });

  it("keeps message content from a moderator who only has moderation:read", async () => {
    await open(GENERAL, ["moderation:read"]);
    await tab("Timeline");
    expect(await screen.findByText(/admin:read/)).toBeInTheDocument();
    expect(screen.queryByRole("list", { name: "Messages" })).toBeNull();
  });

  it("adds an alias and removes one", async () => {
    await open();
    await tab("Aliases");
    const list = await screen.findByRole("list", { name: "Aliases" });
    expect(within(list).getByText("Canonical")).toBeInTheDocument();
    await userEvent.type(screen.getByLabelText("New alias"), "#chat:example.org");
    await userEvent.click(screen.getByRole("button", { name: "Add alias" }));
    expect(await within(list).findByText("#chat:example.org")).toBeInTheDocument();

    await userEvent.click(
      screen.getByRole("button", { name: "Remove #announcements:example.org" }),
    );
    await userEvent.click(await screen.findByRole("button", { name: "Remove alias" }));
    await waitFor(() => expect(within(list).queryByText("#announcements:example.org")).toBeNull());
  });

  it("says why an alias was refused", async () => {
    await open();
    await tab("Aliases");
    await screen.findByRole("list", { name: "Aliases" });
    await userEvent.type(screen.getByLabelText("New alias"), "#general:example.org");
    await userEvent.click(screen.getByRole("button", { name: "Add alias" }));
    expect(await screen.findByText(/already in use/)).toBeInTheDocument();
  });

  it("prunes forked forward extremities to one", async () => {
    await open();
    await tab("Extremities");
    const table = await screen.findByRole("table", { name: "Forward extremities" });
    expect(within(table).getAllByRole("row")).toHaveLength(3);
    await userEvent.click(screen.getByRole("button", { name: "Prune to one" }));
    await waitFor(() => expect(within(table).getAllByRole("row")).toHaveLength(2));
    expect(screen.queryByRole("button", { name: "Prune to one" })).toBeNull();
  });

  it("quarantines all of a room's media as a task and says what it did", async () => {
    await open();
    await tab("Media");
    await screen.findByRole("list", { name: "Room media" });
    await userEvent.click(screen.getByRole("button", { name: "Quarantine all" }));
    await userEvent.click(await screen.findByRole("button", { name: "Quarantine media" }));
    expect(
      await screen.findByText("Quarantined 2 items; 0 already were, 1 protected.", {}, TASK_WAIT),
    ).toBeInTheDocument();
  }, 20_000);

  it("joins a local user to the room", async () => {
    await open();
    await userEvent.click(screen.getByRole("button", { name: "Join a user" }));
    await userEvent.type(await screen.findByLabelText("User ID"), "@bob:example.org");
    await userEvent.click(screen.getByRole("button", { name: "Join user" }));
    expect(await screen.findByText("@bob:example.org")).toBeInTheDocument();
  });

  it("purges history before a date and follows the task", async () => {
    await open();
    await userEvent.click(screen.getByRole("button", { name: "Purge history" }));
    const dialog = await screen.findByRole("dialog");
    fireEvent.change(within(dialog).getByLabelText("Purge messages sent before"), {
      target: { value: "2999-01-01T00:00" },
    });
    await userEvent.click(within(dialog).getByRole("checkbox", { name: /own users/ }));
    await userEvent.click(within(dialog).getByRole("button", { name: "Purge history" }));
    expect(await within(dialog).findByText(/^Purged 30 events/, {}, TASK_WAIT)).toBeInTheDocument();
  }, 20_000);

  it("deletes the room only once its name is typed, and goes back to the rooms", async () => {
    const { router } = await open();
    await userEvent.click(screen.getByRole("button", { name: "Delete room" }));
    const dialog = await screen.findByRole("dialog");
    const confirm = within(dialog).getByRole("button", { name: "Delete room" });
    expect(confirm).toBeDisabled();
    await userEvent.type(within(dialog).getByLabelText(/to confirm/), "General");
    expect(confirm).toBeEnabled();
    await userEvent.click(confirm);
    await waitFor(() => expect(router.state.location.pathname).toBe("/rooms"), TASK_WAIT);
  }, 20_000);
});
