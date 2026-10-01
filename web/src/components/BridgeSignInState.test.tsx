import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { signIn, signOut } from "@/lib/auth";
import { BridgeSignInState } from "./BridgeSignInState";

function renderState(appserviceId: string, defaultUserId?: string) {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={client}>
      <BridgeSignInState appserviceId={appserviceId} defaultUserId={defaultUserId} />
    </QueryClientProvider>,
  );
}

async function check(userId: string) {
  const input = await screen.findByLabelText("Matrix user");
  await userEvent.clear(input);
  await userEvent.type(input, userId);
  await userEvent.click(screen.getByRole("button", { name: "Check" }));
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("Who has signed in to a bridge", () => {
  it("asks a shared bridge about the operator first, and says who is signed in as what", async () => {
    renderState("whatsapp", "@alice:example.org");
    // A shared bridge has to be told whom to ask about; the box starts with the operator.
    expect(await screen.findByText(/Many people can use this bridge/)).toBeInTheDocument();
    expect(screen.getByLabelText("Matrix user")).toHaveValue("@alice:example.org");
    await userEvent.click(screen.getByRole("button", { name: "Check" }));
    expect(await screen.findByText("+1 555-123-4567")).toBeInTheDocument();
    expect(screen.getByRole("list", { name: "Sign-ins of @alice:example.org" })).toHaveTextContent(
      /Signed in as \+1 555-123-4567 since/,
    );
    expect(screen.getByText(/The bridge's answer from/)).toBeInTheDocument();

    await check("@bob:example.org");
    expect(await screen.findByText(/is not signed in\./)).toHaveTextContent(
      "@bob:example.org is not signed in.",
    );
  });

  it("says when the bridge could not be asked", async () => {
    renderState("signal", "@alice:example.org");
    await screen.findByText(/Many people can use this bridge/);
    await userEvent.click(screen.getByRole("button", { name: "Check" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Could not ask the bridge: the server could not reach the bridge: connection refused",
    );
  });

  it("says a bridge without a provisioning API keeps that itself, and asks nothing more", async () => {
    server.use(
      http.get("*/api/v1/appservices/:id/logins", () =>
        HttpResponse.json({
          appservice_id: "irc",
          bridge_type: "heisenbridge",
          provisioning_api: "none",
          supported: false,
          reason: "heisenbridge has no provisioning API: its networks live in its control room.",
          user_id: null,
          logins: [],
          cached: false,
        }),
      ),
    );
    renderState("irc", "@alice:example.org");
    expect(
      await screen.findByText("This bridge keeps who has signed in itself."),
    ).toBeInTheDocument();
    expect(screen.getByText(/its networks live in its control room/)).toBeInTheDocument();
    expect(screen.queryByLabelText("Matrix user")).not.toBeInTheDocument();
  });

  it("shows a per-user instance's owner without being told, and a login in trouble", async () => {
    server.use(
      http.get("*/api/v1/appservices/:id/logins", () =>
        HttpResponse.json({
          appservice_id: "whatsapp-carol",
          bridge_type: "mautrix-whatsapp",
          provisioning_api: "mautrix_v3",
          supported: true,
          user_id: "@carol:example.org",
          signed_in: true,
          logins: [
            {
              user_id: "@carol:example.org",
              remote_id: "15559876543",
              remote_name: null,
              state: "bad_credentials",
              state_reason: "wa-logged-out",
              since: "2026-09-30T12:00:00.000Z",
            },
          ],
          checked_at: "2026-10-01T00:00:00.000Z",
          cached: true,
        }),
      ),
    );
    renderState("whatsapp-carol");
    expect(await screen.findByText("15559876543")).toBeInTheDocument();
    expect(screen.getByText("bad credentials")).toBeInTheDocument();
    expect(screen.getByText("wa-logged-out")).toBeInTheDocument();
    expect(screen.getByText(/kept for up to 30 seconds/)).toBeInTheDocument();
    // Its owner was asked about without a box to say whom.
    expect(screen.queryByLabelText("Matrix user")).not.toBeInTheDocument();
  });
});
