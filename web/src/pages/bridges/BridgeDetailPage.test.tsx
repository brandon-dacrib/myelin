import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, within } from "@testing-library/react";
import { signIn, signOut } from "@/lib/auth";
import { renderBridgesRoute } from "./test-utils";
import { BridgeDetailPage } from "./BridgeDetailPage";

function renderBridge(id: string) {
  return renderBridgesRoute("/bridges/$bridgeId", BridgeDetailPage, `/bridges/${id}`);
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

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
});
