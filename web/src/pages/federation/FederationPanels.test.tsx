import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { signIn, signOut } from "@/lib/auth";
import { cachedKeys } from "@/mocks/data/federation";
import { renderRoutes } from "@/test/render-route";
import { Toaster } from "@/components/ui/toast/Toaster";
import { FederationDestinationPage } from "../FederationDestinationPage";
import { FederationPage } from "../FederationPage";

function open(path: string) {
  return renderRoutes(
    [
      {
        path: "/federation/$serverName",
        component: () => (
          <>
            <FederationDestinationPage />
            <Toaster />
          </>
        ),
      },
      { path: "/federation", component: FederationPage },
    ],
    path,
    ["/rooms/$roomId", "/tasks/$taskId"],
  );
}

describe("Federation keys and shared rooms", () => {
  beforeEach(async () => {
    await signIn();
  });
  afterEach(() => signOut());

  it("lists the rooms shared with a destination, most of its users first", async () => {
    open("/federation/matrix.org");
    const table = await screen.findByRole("table", { name: "Rooms shared with matrix.org" });
    const row = within(table).getByRole("link", { name: "General" }).closest("tr")!;
    expect(within(row).getByText("37")).toBeInTheDocument();
    expect(within(row).getByText("214")).toBeInTheDocument();
  });

  it("shows the cached keys, and fetches them again as a task", async () => {
    open("/federation/gnome.org");
    // Nothing cached for gnome.org yet.
    expect(await screen.findByText("No keys cached")).toBeInTheDocument();
    const user = userEvent.setup();
    await user.click(screen.getByRole("button", { name: "Fetch keys again" }));
    expect(
      await screen.findByRole("progressbar", { name: "Fetching gnome.org's keys" }),
    ).toBeInTheDocument();
    expect(
      await screen.findByText("Fetched gnome.org's keys again", {}, { timeout: 5000 }),
    ).toBeInTheDocument();
    const table = await screen.findByRole("table", { name: "gnome.org's signing keys" });
    expect(within(table).getByText("ed25519:gnome_2026")).toBeInTheDocument();
    expect(cachedKeys("gnome.org")).toBeDefined();
  });

  it("says so when a destination's keys cannot be fetched", async () => {
    open("/federation/mozilla.org");
    const user = userEvent.setup();
    await user.click(await screen.findByRole("button", { name: "Fetch keys again" }));
    expect(
      await screen.findByText("Couldn't fetch mozilla.org's keys", {}, { timeout: 5000 }),
    ).toBeInTheDocument();
  });

  it("read-only operators see the keys but cannot fetch them", async () => {
    signOut();
    await signIn(["admin:read"]);
    open("/federation/matrix.org");
    const table = await screen.findByRole("table", { name: "matrix.org's signing keys" });
    expect(within(table).getByText("ed25519:matrix_2026")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Fetch keys again" })).toBeDisabled();
  });

  it("shows this server's own signing keys on the Federation page", async () => {
    open("/federation");
    const table = await screen.findByRole("table", { name: "This server's signing keys" });
    expect(within(table).getByText("ed25519:a_1727000000000_q2V9Zw")).toBeInTheDocument();
    expect(within(table).getByText("Until rotated")).toBeInTheDocument();
  });
});
