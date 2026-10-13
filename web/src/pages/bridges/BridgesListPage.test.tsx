import { afterEach, beforeEach, describe, expect, it } from "vitest";
import userEvent from "@testing-library/user-event";
import { screen, within } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { signIn, signOut } from "@/lib/auth";
import { server } from "@/mocks/node";
import { renderBridgesRoute } from "./test-utils";
import { BridgesListPage } from "./BridgesListPage";

beforeEach(async () => {
  await signIn();
});
afterEach(() => {
  server.resetHandlers();
  signOut();
});

describe("Bridges list", () => {
  it("says what waits to be sent to each bridge, and explains it", async () => {
    renderBridgesRoute("/bridges/registrations", BridgesListPage, "/bridges/registrations");
    // The mock's Telegram bridge has one transaction waiting six minutes; Signal has two that
    // ran out of attempts; WhatsApp has nothing waiting.
    expect(await screen.findByTestId("queue-telegram")).toHaveTextContent("1 waiting, oldest 6m");
    expect(screen.getByTestId("queue-signal")).toHaveTextContent("2 failed");
    expect(screen.getByTestId("queue-whatsapp")).toHaveTextContent("Up to date");
    // The columns are explained behind a disclosure, not above the list.
    expect(screen.getByText(/what the server has queued for a bridge/)).not.toBeVisible();
    await userEvent.click(screen.getByText("What the columns mean"));
    expect(screen.getByText(/what the server has queued for a bridge/)).toBeVisible();
  });

  it("marks the server's own bridge manager as built in, not as a bridge of unknown health", async () => {
    server.use(
      http.get("/api/v1/appservices", () =>
        HttpResponse.json({
          items: [{ id: "myelin-bridges", sender_localpart: "bridges", health: "unknown" }],
          next_cursor: null,
          prev_cursor: null,
        }),
      ),
    );
    renderBridgesRoute("/bridges/registrations", BridgesListPage, "/bridges/registrations");
    expect(
      await screen.findByRole("link", { name: "This server's bridge manager" }),
    ).toBeInTheDocument();
    expect(screen.getByText("Built in: runs the bridges offered here")).toBeInTheDocument();
    const row = screen.getByRole("row", { name: /bridge manager/ });
    expect(within(row).getByText("Built in")).toBeInTheDocument();
    expect(within(row).queryByText("Unknown")).not.toBeInTheDocument();
  });

  it("marks a bridge whose double puppeting the server folded into its row", async () => {
    let asked: URLSearchParams | undefined;
    server.use(
      http.get("/api/v1/appservices", ({ request }) => {
        asked = new URL(request.url).searchParams;
        return HttpResponse.json({
          items: [
            {
              id: "signal-brandon",
              sender_localpart: "signal-brandon",
              url: "http://signal-brandon.bridges.svc:29328",
              health: "healthy",
              double_puppet: "signal-brandon-doublepuppet",
              double_puppet_for: null,
            },
            {
              id: "slack-brandon",
              sender_localpart: "slack-brandon",
              url: "http://slack-brandon.bridges.svc:29335",
              health: "healthy",
              double_puppet: null,
              double_puppet_for: null,
            },
          ],
          next_cursor: null,
          prev_cursor: null,
        });
      }),
    );
    renderBridgesRoute("/bridges/registrations", BridgesListPage, "/bridges/registrations");
    const signal = await screen.findByRole("row", { name: /Signal Brandon/ });
    expect(within(signal).getByText(/Double puppeting on/)).toBeInTheDocument();
    // The server folds: the page asks it to, twenty bridges at a time.
    expect(asked?.get("fold_double_puppets")).toBe("true");
    expect(asked?.get("limit")).toBe("20");
    expect(screen.getAllByRole("row")).toHaveLength(3); // the header and two bridges
    const slack = screen.getByRole("row", { name: /Slack Brandon/ });
    expect(within(slack).queryByText(/Double puppeting on/)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: /All/ })).toHaveTextContent("2");
  });

  it("keeps a bridge and its double puppeting one row where unfolded paging would split them", async () => {
    // Twenty-one bridges, each with a double-puppeting registration, as the real server holds
    // them (in id order). Twenty to a page unfolded, bridge-11's registration would be on page 2
    // and bridge-10 on page 1 with its own beside it; folded by the mock server as by the real
    // one, page 1 is twenty bridges and each says it double puppets.
    const { appservices } = await import("@/mocks/data/appservices");
    const saved = appservices.splice(0, appservices.length);
    try {
      for (let n = 0; n < 21; n++) {
        const id = `bridge-${String(n).padStart(2, "0")}`;
        appservices.push(
          {
            id,
            sender_localpart: `${id}bot`,
            url: `http://${id}.bridges.svc:29318`,
            health: "healthy",
            double_puppet_for: null,
          },
          {
            id: `${id}-doublepuppet`,
            sender_localpart: `${id}-doublepuppet`,
            url: null,
            health: "unknown",
            double_puppet_for: id,
          },
        );
      }
      renderBridgesRoute("/bridges/registrations", BridgesListPage, "/bridges/registrations");
      await screen.findByRole("row", { name: /Bridge 00/ });
      const rows = screen.getAllByRole("row").slice(1);
      expect(rows).toHaveLength(20);
      for (const row of rows) {
        expect(within(row).getByText(/Double puppeting on/)).toBeInTheDocument();
      }
      expect(screen.queryByText(/Double puppeting for/)).not.toBeInTheDocument();
      expect(screen.getByRole("button", { name: /All/ })).toHaveTextContent("20");
      await userEvent.click(screen.getByRole("button", { name: /next/i }));
      const last = await screen.findByRole("row", { name: /Bridge 20/ });
      expect(within(last).getByText(/Double puppeting on/)).toBeInTheDocument();
      expect(screen.getAllByRole("row")).toHaveLength(2); // the header and bridge-20
    } finally {
      appservices.splice(0, appservices.length, ...saved);
    }
  });

  it("keeps a double-puppeting registration whose bridge is gone, and one with no URL, as rows", async () => {
    server.use(
      http.get("/api/v1/appservices", () =>
        HttpResponse.json({
          items: [
            {
              id: "signal-brandon-doublepuppet",
              sender_localpart: "signal-brandon-doublepuppet",
              url: null,
              health: "unknown",
              double_puppet_for: "signal-brandon",
              double_puppet: null,
            },
            {
              id: "my-puppets",
              sender_localpart: "puppets",
              url: null,
              health: "unknown",
              double_puppet_for: null,
              double_puppet: null,
            },
          ],
          next_cursor: null,
          prev_cursor: null,
        }),
      ),
    );
    renderBridgesRoute("/bridges/registrations", BridgesListPage, "/bridges/registrations");
    expect(
      await screen.findByRole("link", { name: "Double puppeting for Signal Brandon" }),
    ).toBeInTheDocument();
    const dp = screen.getByRole("row", { name: /Double puppeting for Signal Brandon/ });
    expect(within(dp).getByText("Built in")).toBeInTheDocument();
    expect(within(dp).queryByText("Unknown")).not.toBeInTheDocument();
    const plain = screen.getByRole("row", { name: /My Puppets/ });
    expect(within(plain).getByText("Sends nothing")).toBeInTheDocument();
    expect(within(plain).queryByText("Unknown")).not.toBeInTheDocument();
    // Neither is counted as a bridge of unknown health.
    expect(screen.queryByRole("button", { name: /Unknown/ })).not.toBeInTheDocument();
  });
});
