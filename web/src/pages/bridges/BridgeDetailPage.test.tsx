import { afterEach, beforeEach, describe, expect, it } from "vitest";
import userEvent from "@testing-library/user-event";
import { screen, waitFor, within } from "@testing-library/react";
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
    // The mock server's Slack bridge has `slack-doublepuppet`, which it names in `double_puppet`;
    // nothing is probed by id.
    let probed = false;
    server.use(
      http.get("/api/v1/appservices/slack-doublepuppet", () => {
        probed = true;
        return HttpResponse.json({});
      }),
    );
    renderBridge("slack");
    const link = await screen.findByRole("link", { name: "slack-doublepuppet" });
    expect(link).toHaveAttribute("href", "/bridges/slack-doublepuppet");
    expect(screen.getByText(/On: it posts as its owner with/)).toBeInTheDocument();
    expect(probed).toBe(false);
  });

  it("reads the double-puppeting registration's name from the server, not from its id", async () => {
    server.use(
      http.get("/api/v1/appservices/telegram", () =>
        HttpResponse.json({
          id: "telegram",
          sender_localpart: "telegrambot",
          url: "http://mautrix-telegram.bridges.svc:29317",
          health: "healthy",
          double_puppet: "telegram-puppets",
          double_puppet_for: null,
        }),
      ),
    );
    renderBridge("telegram");
    const link = await screen.findByRole("link", { name: "telegram-puppets" });
    expect(link).toHaveAttribute("href", "/bridges/telegram-puppets");
  });

  it("says double puppeting is off when the bridge has no such registration", async () => {
    renderBridge("telegram");
    expect(await screen.findByText(/Off: it posts as its own ghosts only/)).toBeInTheDocument();
  });

  it("shows a double-puppeting registration as its bridge's key, not as a bridge", async () => {
    let healthAsked = false;
    server.use(
      // Named for its bridge by `double_puppet_for`, whatever its id.
      http.get("/api/v1/appservices/tg-key", () =>
        HttpResponse.json({
          id: "tg-key",
          sender_localpart: "tg-key",
          url: null,
          health: "unknown",
          double_puppet_for: "telegram",
          double_puppet: null,
          namespaces: { users: [{ regex: "@brandon:example\\.org", exclusive: false }] },
        }),
      ),
      http.get("/api/v1/appservices/tg-key/health", () => {
        healthAsked = true;
        return HttpResponse.json({ status: "unknown" });
      }),
    );
    renderBridge("tg-key");
    expect(
      await screen.findByRole("heading", { level: 1, name: "Double puppeting for Telegram" }),
    ).toBeInTheDocument();
    expect(screen.getByText(/This is not a bridge/)).toBeInTheDocument();
    expect(screen.getByText("Built in")).toBeInTheDocument();
    expect(screen.getByText("@brandon:example\\.org")).toBeInTheDocument();
    // Its bridge is linked, by the bridge's name, once its record is read.
    const links = await screen.findAllByRole("link", { name: /^Telegram$/ });
    for (const link of links) expect(link).toHaveAttribute("href", "/bridges/telegram");
    // Nothing that belongs to a bridge the server delivers to.
    expect(screen.queryByText("Unknown")).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Test connection" })).not.toBeInTheDocument();
    expect(screen.queryByRole("tab")).not.toBeInTheDocument();
    expect(screen.queryByText(/^Off:/)).not.toBeInTheDocument();
    expect(healthAsked).toBe(false);
  });

  it("says a double-puppeting registration's bridge is gone, and offers to remove it", async () => {
    let deleted = false;
    server.use(
      http.get("/api/v1/appservices/gone-doublepuppet", () =>
        HttpResponse.json({
          id: "gone-doublepuppet",
          sender_localpart: "gone-doublepuppet",
          url: null,
          health: "unknown",
          double_puppet_for: "gone",
          double_puppet: null,
        }),
      ),
      http.get("/api/v1/appservices/gone", () =>
        HttpResponse.json(
          { type: "urn:hs:problem:not-found", title: "Not found", status: 404 },
          { status: 404 },
        ),
      ),
      http.delete("/api/v1/appservices/gone-doublepuppet", () => {
        deleted = true;
        return new HttpResponse(null, { status: 204 });
      }),
    );
    renderBridge("gone-doublepuppet");
    expect(
      await screen.findByText(/Its bridge has been removed, so nothing uses it any more/),
    ).toBeInTheDocument();
    expect(screen.queryByRole("link", { name: /^Gone$/ })).not.toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Remove registration" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Remove registration" }));
    await waitFor(() => expect(deleted).toBe(true));
  });
});
