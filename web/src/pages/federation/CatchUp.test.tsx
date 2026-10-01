import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, within } from "@testing-library/react";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { FederationDestinationPage } from "../FederationDestinationPage";
import { FederationPage } from "../FederationPage";

function open(path: string) {
  return renderRoutes(
    [
      { path: "/federation/$serverName", component: FederationDestinationPage },
      { path: "/federation", component: FederationPage },
    ],
    path,
    ["/rooms/$roomId", "/tasks/$taskId", "/configuration/$section"],
  );
}

describe("Catch-up on the Federation pages", () => {
  beforeEach(async () => {
    await signIn();
  });
  afterEach(() => signOut());

  it("marks a destination in catch-up in the list, with nothing queued for it", async () => {
    open("/federation");
    const table = await screen.findByRole("table", { name: "Federation destinations" });
    const row = within(table).getByRole("link", { name: "kde.org" }).closest("tr")!;
    expect(within(row).getByText("Failing")).toBeInTheDocument();
    expect(within(row).getByText(/Catching up since/)).toBeInTheDocument();
    expect(within(row).getByText("not queued")).toBeInTheDocument();
    // The key above the table says what catch-up is, opened because a destination is in it.
    expect(screen.getByText(/1 server is catching up/)).toBeInTheDocument();
    expect(screen.getByText(/so this server stopped queuing for it/)).toBeVisible();
  });

  it("explains catch-up on the destination page, with the queue limit and where to change it", async () => {
    open("/federation/kde.org");
    const notice = await screen.findByRole("region", { name: /Catching up since/ });
    expect(within(notice).getByText(/longer than its queue holds \(10,000 events\)/)).toBeVisible();
    expect(within(notice).getByText(/fetches the history in between itself/)).toBeVisible();
    const link = within(notice).getByRole("link", { name: "Max queued PDUs per destination" });
    expect(link).toHaveAttribute(
      "href",
      "/configuration/federation#setting-max_queued_pdus_per_destination",
    );
    expect(screen.getByText("Not queued while catching up")).toBeInTheDocument();
  });

  it("says nothing about catch-up for a destination not in it, and names its attempts", async () => {
    open("/federation/mozilla.org");
    expect(await screen.findByText("Last attempt")).toBeInTheDocument();
    expect(screen.getByText("Next attempt")).toBeInTheDocument();
    expect(screen.getByText("5 minutes")).toBeInTheDocument();
    expect(screen.queryByRole("region", { name: /Catching up/ })).not.toBeInTheDocument();
    expect(screen.getByText("42")).toBeInTheDocument();
  });
});
