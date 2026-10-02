import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { BridgeInstance, BridgeOffering } from "@/api/bridges";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { bridgeTypes } from "@/mocks/data/bridge-types";
import { signIn, signOut } from "@/lib/auth";
import { InstanceNextSteps } from "./InstanceNextSteps";

const whatsapp: BridgeOffering = {
  type: "mautrix-whatsapp",
  name: "WhatsApp",
  mode: "per_user",
  enabled: true,
  runtime: "cluster",
  front_door: "@whatsappbot:example.org",
  instances: {},
};

const catalogue = bridgeTypes.find((t) => t.id === "mautrix-whatsapp");

/**
 * An instance as the mock server would list it. `appservice_id` is what the logins mock keys on:
 * `whatsapp-alice` answers signed in, anyone else not; `health: "down"` answers an error.
 */
function instance(userId: string, fields: Partial<BridgeInstance> = {}): BridgeInstance {
  const localpart = userId.slice(1).split(":")[0];
  return {
    type: "mautrix-whatsapp",
    user_id: userId,
    state: "ready",
    appservice_id: `whatsapp-${localpart}`,
    bot: `@whatsappbot_${localpart}:example.org`,
    health: "healthy",
    reason: null,
    deployment: null,
    created_at: new Date().toISOString(),
    ready_at: new Date().toISOString(),
    ...fields,
  };
}

function renderSteps(i: BridgeInstance, offering = whatsapp) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <InstanceNextSteps instance={i} offering={offering} type={catalogue} />
    </QueryClientProvider>,
  );
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("InstanceNextSteps", () => {
  it("tells the operator what to relay once the bridge is ready and the person has not signed in", async () => {
    renderSteps(instance("@carol:example.org"));
    expect(
      await screen.findByText("Ready, and @carol:example.org has not signed in yet."),
    ).toBeInTheDocument();
    expect(screen.getByText(/Tell them: their WhatsApp bridge is ready/)).toBeInTheDocument();
    // Their own bot, copyable, and the catalogue's steps with it filled in.
    expect(
      screen.getByRole("button", { name: "Copy @whatsappbot_carol:example.org" }),
    ).toBeInTheDocument();
    const steps = screen.getByRole("list");
    expect(steps).toHaveTextContent(
      "Start a direct chat with @whatsappbot_carol:example.org and send login qr",
    );
    expect(screen.getByText(/WhatsApp unlinks the bridge/)).toBeInTheDocument();
    expect(screen.queryByText(/This is you/)).toBeNull();
  });

  it("copies the steps as one message to paste to the person", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });
    renderSteps(instance("@carol:example.org"));
    await userEvent.click(
      await screen.findByRole("button", { name: "Copy as a message to send them" }),
    );
    expect(writeText).toHaveBeenCalledTimes(1);
    const text = writeText.mock.calls[0][0] as string;
    expect(text).toMatch(
      /^Your WhatsApp bridge is ready\. Its bot, @whatsappbot_carol:example\.org/,
    );
    expect(text).toContain("1. Start a direct chat with @whatsappbot_carol:example.org");
    expect(await screen.findByRole("button", { name: "Copied" })).toBeInTheDocument();
  });

  it("says 'this is you' for the operator's own bridge, with the command to send", async () => {
    // The mock session is @ops:example.org.
    renderSteps(instance("@ops:example.org"));
    const notice = await screen.findByText(/This is you: accept the invite from/);
    expect(notice).toHaveTextContent("@whatsappbot_ops:example.org");
    expect(notice).toHaveTextContent("and send login qr");
    expect(screen.getByRole("button", { name: "Copy the steps" })).toBeInTheDocument();
  });

  it("hides the steps once they have signed in", async () => {
    renderSteps(instance("@alice:example.org"));
    expect(await screen.findByText(/Signed in as/)).toHaveTextContent("+1 555-123-4567");
    expect(screen.queryByRole("list")).toBeNull();
    expect(screen.queryByText(/Tell them/)).toBeNull();
  });

  it("keeps the steps when the bridge could not be asked, and says so", async () => {
    server.use(
      http.get("/api/v1/appservices/:id/logins", ({ params }) =>
        HttpResponse.json({
          appservice_id: String(params.id),
          bridge_type: "mautrix-whatsapp",
          provisioning_api: "mautrix_v3",
          supported: true,
          cached: false,
          logins: [],
          user_id: "@carol:example.org",
          error: {
            status: 502,
            reason: "unreachable",
            detail: "the server could not reach the bridge: connection refused",
          },
        }),
      ),
    );
    renderSteps(instance("@carol:example.org", { health: "down" }));
    expect(
      await screen.findByText(/could not be asked \(.*connection refused\)/),
    ).toBeInTheDocument();
    expect(screen.getByRole("list")).toHaveTextContent("login qr");
  });

  it("says what is happening while the bridge is on its way, and that the steps come later", () => {
    renderSteps(
      instance("@carol:example.org", {
        state: "starting",
        appservice_id: null,
        reason: "Waiting for the bridge to answer this server's ping.",
        ready_at: null,
      }),
    );
    expect(screen.getByText("Starting")).toBeInTheDocument();
    expect(screen.getByText(/Setting up @carol:example.org's WhatsApp bridge/)).toBeInTheDocument();
    expect(
      screen.getByText("Waiting for the bridge to answer this server's ping."),
    ).toBeInTheDocument();
    expect(screen.getByText(/the steps appear here too/)).toHaveTextContent(
      "@whatsappbot_carol:example.org",
    );
    expect(screen.queryByRole("list")).toBeNull();
  });

  it("tells the operator to run a bridge that runs elsewhere", () => {
    renderSteps(instance("@alice:example.org", { state: "starting", appservice_id: null }), {
      ...whatsapp,
      type: "mautrix-imessage",
      name: "iMessage",
      runtime: "elsewhere",
    });
    expect(screen.getByText(/use Files in the table/)).toBeInTheDocument();
    expect(screen.getByText(/Nothing to tell @alice:example.org yet/)).toBeInTheDocument();
  });

  it("has nothing to tell anyone for a failed bridge, and says what to do instead", () => {
    renderSteps(
      instance("@dave:example.org", {
        state: "failed",
        reason: "The cluster could not pull the bridge's image (ImagePullBackOff).",
      }),
    );
    expect(
      screen.getByText(/It stopped on the way: The cluster could not pull/),
    ).toBeInTheDocument();
    expect(screen.getByText(/Retry it from the table/)).toBeInTheDocument();
  });

  it("names the bot from the front door when the server does not say", async () => {
    renderSteps(instance("@carol:example.org", { bot: null }));
    expect(
      await screen.findByRole("button", { name: "Copy @whatsappbot_carol:example.org" }),
    ).toBeInTheDocument();
  });
});
