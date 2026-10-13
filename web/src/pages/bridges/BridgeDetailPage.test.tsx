import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, within } from "@testing-library/react";
import { http, HttpResponse } from "msw";
import { signIn, signOut } from "@/lib/auth";
import { server } from "@/mocks/node";
import { renderBridgesRoute } from "./test-utils";
import { BridgeDetailPage } from "./BridgeDetailPage";

function renderBridge(id: string) {
  return renderBridgesRoute("/bridges/$bridgeId", BridgeDetailPage, `/bridges/${id}`);
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => {
  server.resetHandlers();
  signOut();
});

describe("Bridge page", () => {
  it("says a bridge registered by hand overlaps the offering, and links to it", async () => {
    // The mock server's `whatsapp` registration was made from the catalogue's WhatsApp entry,
    // and WhatsApp is offered: the demo's case (RFC 0017 section 6).
    renderBridge("whatsapp");
    const line = await screen.findByTestId("overlaps-offering");
    expect(line).toHaveTextContent(/WhatsApp is offered on this server now/);
    expect(line).toHaveTextContent(/messaging @whatsappbot:example.org/);
    expect(within(line).getByRole("link", { name: "The WhatsApp offering" })).toHaveAttribute(
      "href",
      "/bridges/offerings/mautrix-whatsapp",
    );
  });

  it("says nothing of the kind for a bridge no offering overlaps", async () => {
    renderBridge("telegram");
    expect(await screen.findByRole("heading", { level: 1 })).toBeInTheDocument();
    expect(screen.queryByTestId("overlaps-offering")).toBeNull();
  });

  it("says a bridge double puppets, and links its double-puppeting registration", async () => {
    const dp = {
      id: "whatsapp-doublepuppet",
      sender_localpart: "whatsapp-doublepuppet",
      url: null,
      health: "unknown",
    };
    server.use(http.get("/api/v1/appservices/whatsapp-doublepuppet", () => HttpResponse.json(dp)));
    renderBridge("whatsapp");
    const link = await screen.findByRole("link", { name: "whatsapp-doublepuppet" });
    expect(link).toHaveAttribute("href", "/bridges/whatsapp-doublepuppet");
    expect(screen.getByText(/On: it posts as its owner with/)).toBeInTheDocument();
  });

  it("says double puppeting is off when the bridge has no such registration", async () => {
    renderBridge("telegram");
    expect(await screen.findByText(/Off: it posts as its own ghosts only/)).toBeInTheDocument();
  });

  it("links a double-puppeting registration back to its bridge", async () => {
    server.use(
      http.get("/api/v1/appservices/telegram-doublepuppet", () =>
        HttpResponse.json({
          id: "telegram-doublepuppet",
          sender_localpart: "telegram-doublepuppet",
          url: null,
          health: "unknown",
        }),
      ),
    );
    renderBridge("telegram-doublepuppet");
    const link = await screen.findByRole("link", { name: "telegram" });
    expect(link).toHaveAttribute("href", "/bridges/telegram");
    expect(screen.queryByText(/^Off:/)).not.toBeInTheDocument();
  });
});
