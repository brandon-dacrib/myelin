import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, within } from "@testing-library/react";
import { setDeploymentTarget } from "@/mocks/data/bridge-offerings";
import { signIn, signOut } from "@/lib/auth";
import { renderBridgesRoute } from "./test-utils";
import { BridgeOfferingsPage } from "./BridgeOfferingsPage";

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("Offered bridges", () => {
  it("lists each offering with where it runs, its bot and how its bridges are doing", async () => {
    renderBridgesRoute("/bridges", BridgeOfferingsPage, "/bridges");
    const table = await screen.findByRole("table");
    const whatsapp = within(
      (await within(table).findByRole("link", { name: "WhatsApp" })).closest("tr")!,
    );
    expect(whatsapp.getByText("Runs in this cluster")).toBeInTheDocument();
    expect(whatsapp.getByText("@whatsappbot:example.org")).toBeInTheDocument();
    expect(whatsapp.getByText(/ready/).closest("span")).toHaveTextContent("2 ready");
    expect(whatsapp.getByText(/failed/).closest("span")).toHaveTextContent("1 failed");

    const imessage = within(within(table).getByRole("link", { name: "iMessage" }).closest("tr")!);
    expect(imessage.getByText("Runs elsewhere")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Offer a bridge" })).toBeEnabled();
    expect(screen.getByRole("link", { name: "Registrations" })).toBeInTheDocument();
  });

  it("says when this server can't run bridges itself", async () => {
    setDeploymentTarget(false, "Not in Kubernetes.");
    renderBridgesRoute("/bridges", BridgeOfferingsPage, "/bridges");
    expect(await screen.findByText(/Not in Kubernetes\./)).toBeInTheDocument();
    expect(screen.getByText("This server can't run bridges itself.")).toBeInTheDocument();
  });
});
