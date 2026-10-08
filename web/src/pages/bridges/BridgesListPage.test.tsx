import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import { signIn, signOut } from "@/lib/auth";
import { renderBridgesRoute } from "./test-utils";
import { BridgesListPage } from "./BridgesListPage";

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("Bridges list", () => {
  it("says what waits to be sent to each bridge, and explains it", async () => {
    renderBridgesRoute("/bridges/registrations", BridgesListPage, "/bridges/registrations");
    // The mock's Telegram bridge has one transaction waiting six minutes; Signal has two that
    // ran out of attempts; WhatsApp has nothing waiting.
    expect(await screen.findByTestId("queue-telegram")).toHaveTextContent("1 waiting, oldest 6m");
    expect(screen.getByTestId("queue-signal")).toHaveTextContent("2 failed");
    expect(screen.getByTestId("queue-whatsapp")).toHaveTextContent("Up to date");
    expect(screen.getByText(/Waiting to send counts what the server has queued/)).toBeVisible();
  });
});
