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
});
