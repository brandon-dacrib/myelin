import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import { putOffering } from "@/mocks/data/bridge-offerings";
import { signIn, signOut, type Scope } from "@/lib/auth";
import { renderBridgesRoute } from "../test-utils";
import { BridgeOfferingPage } from "./BridgeOfferingPage";

function renderOffering(type: string) {
  return renderBridgesRoute(
    "/bridges/offerings/$type",
    BridgeOfferingPage,
    `/bridges/offerings/${type}`,
  );
}

/**
 * A button in the table. DataTable also renders a phone-width card list (hidden by CSS, which
 * jsdom does not apply), so every row action exists twice in the DOM.
 */
async function tableButton(name: string) {
  const table = await screen.findByRole("table");
  return within(table).findByRole("button", { name });
}

/** The table row for a person, found by their copyable ID. */
async function rowFor(userId: string) {
  const table = await screen.findByRole("table");
  const id = await within(table).findByText(userId, { selector: "span" });
  return within(id.closest("tr") as HTMLElement);
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("Bridge offering page", () => {
  it("says whether each person has signed in to their bridge", async () => {
    renderOffering("mautrix-whatsapp");
    const alice = await rowFor("@alice:example.org");
    expect(await alice.findByText("+1 555-123-4567")).toBeInTheDocument();
    const ops = await rowFor("@ops:example.org");
    expect(await ops.findByText("Not signed in")).toBeInTheDocument();
    // A bridge that is not ready yet is not asked.
    const carol = await rowFor("@carol:example.org");
    expect(carol.getByText("Once it is ready")).toBeInTheDocument();
  });

  it("says what to tell people and shows everybody's bridge, failed first", async () => {
    renderOffering("mautrix-whatsapp");
    expect(await screen.findByRole("heading", { name: "WhatsApp" })).toBeInTheDocument();
    expect(
      screen.getByText(
        "Anyone here can message @whatsappbot:example.org to get their own WhatsApp bridge.",
      ),
    ).toBeInTheDocument();

    const table = await screen.findByRole("table");
    await within(table).findByText("@dave:example.org");
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows[0]).toHaveTextContent("@dave:example.org");
    expect(rows[0]).toHaveTextContent("Failed");
    expect(rows[0]).toHaveTextContent("ImagePullBackOff");
    expect(rows[0]).toHaveTextContent("Degraded");

    // Retry only where there is something to retry.
    expect(within(table).getAllByRole("button", { name: /^Retry / })).toHaveLength(1);
    const alice = await rowFor("@alice:example.org");
    // The state, the pod phase and the time it became ready.
    expect(alice.getAllByText("Ready")).toHaveLength(3);
    expect(alice.getByText("Healthy")).toBeInTheDocument();
  });

  it("hands over an instance's files", async () => {
    renderOffering("mautrix-imessage");
    await screen.findByText(/an administrator runs iMessage for them/);
    await userEvent.click(await tableButton("Files for @alice:example.org"));
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText(/their Mac, signed in to Messages/)).toBeInTheDocument();
    for (const label of [
      "config.yaml",
      "registration.yaml",
      "docker-compose.yaml",
      "Kubernetes manifest (Secret and Bridge)",
    ]) {
      expect(await within(dialog).findByRole("region", { name: label })).toBeInTheDocument();
    }
    expect(within(dialog).getByRole("region", { name: "registration.yaml" })).toHaveTextContent(
      "io.myelin.bridge_instance: @alice:example.org",
    );
  });

  it("adds a bridge for someone, and it appears on its way", async () => {
    renderOffering("mautrix-whatsapp");
    await userEvent.click(await screen.findByRole("button", { name: "Add for a user" }));
    const dialog = await screen.findByRole("dialog");
    const input = within(dialog).getByLabelText(/Matrix ID/);

    await userEvent.type(input, "erin");
    await userEvent.click(within(dialog).getByRole("button", { name: "Add bridge" }));
    expect(await within(dialog).findByText(/A Matrix ID, like/)).toBeInTheDocument();

    await userEvent.clear(input);
    await userEvent.type(input, "@erin:example.org");
    await userEvent.click(within(dialog).getByRole("button", { name: "Add bridge" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

    const erin = await rowFor("@erin:example.org");
    expect(erin.getByText(/Requested|Registered|Deploying/)).toBeInTheDocument();
  });

  it("says the server's reason when it refuses someone", async () => {
    renderOffering("mautrix-whatsapp");
    await userEvent.click(await screen.findByRole("button", { name: "Add for a user" }));
    const dialog = await screen.findByRole("dialog");
    await userEvent.type(within(dialog).getByLabelText(/Matrix ID/), "@eve:elsewhere.net");
    await userEvent.click(within(dialog).getByRole("button", { name: "Add bridge" }));
    expect(await within(dialog).findByText(/not a user on this server/)).toBeInTheDocument();
  });

  it("removes a person's bridge after saying their sign-ins go with it", async () => {
    renderOffering("mautrix-whatsapp");
    await userEvent.click(await tableButton("Remove @carol:example.org"));
    const dialog = await screen.findByRole("dialog");
    expect(dialog).toHaveTextContent("sign-in");
    await userEvent.click(within(dialog).getByRole("button", { name: "Remove bridge" }));
    await waitFor(() =>
      expect(within(screen.getByRole("table")).queryByText("@carol:example.org")).toBeNull(),
    );
  });

  it("retries a failed bridge", async () => {
    renderOffering("mautrix-whatsapp");
    await userEvent.click(await tableButton("Retry @dave:example.org"));
    const dave = await rowFor("@dave:example.org");
    await waitFor(() => expect(dave.queryByText("Failed")).toBeNull());
  });

  it("asks for the name before removing everybody's bridge with the offering", async () => {
    const { router } = renderOffering("mautrix-whatsapp");
    await userEvent.click(await screen.findByRole("button", { name: "Stop offering" }));
    let dialog = await screen.findByRole("dialog");
    await userEvent.click(within(dialog).getByRole("button", { name: "Stop offering" }));

    dialog = await screen.findByRole("dialog", { name: /Remove everyone's WhatsApp bridge/ });
    expect(dialog).toHaveTextContent("4 instances are still running");
    const confirm = within(dialog).getByRole("button", { name: "Remove all and stop offering" });
    expect(confirm).toBeDisabled();
    await userEvent.type(within(dialog).getByLabelText("Type WhatsApp to confirm"), "WhatsApp");
    expect(confirm).toBeEnabled();
    await userEvent.click(confirm);
    await waitFor(() => expect(router.state.location.pathname).toBe("/bridges"));
  });

  it("changes who can have one, and the page says so", async () => {
    renderOffering("mautrix-whatsapp");
    await userEvent.click(await screen.findByRole("button", { name: "Edit settings" }));
    const dialog = await screen.findByRole("dialog", { name: "WhatsApp settings" });
    await userEvent.click(within(dialog).getByRole("radio", { name: /Only the people I list/ }));
    const save = within(dialog).getByRole("button", { name: "Save settings" });
    await userEvent.click(save);
    expect(await within(dialog).findByText(/List at least one person/)).toBeInTheDocument();

    await userEvent.type(
      within(dialog).getByLabelText("People who can have one"),
      "@alice:example.org, @ops:example.org",
    );
    await userEvent.click(save);
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(await screen.findByText("@alice:example.org, @ops:example.org")).toBeInTheDocument();
    expect(
      screen.getByText(
        "Anyone allowed can message @whatsappbot:example.org to get their own WhatsApp bridge.",
      ),
    ).toBeInTheDocument();
  });

  it("shows a shared bridge as one status panel, not a table", async () => {
    putOffering("heisenbridge", { runtime: "cluster", enabled: true });
    renderOffering("heisenbridge");
    expect(await screen.findByRole("heading", { name: "The bridge" })).toBeInTheDocument();
    expect(screen.getByText(/serves everyone allowed/)).toBeInTheDocument();
    expect(await screen.findByRole("button", { name: /^Files for / })).toBeInTheDocument();
    expect(screen.queryByRole("table")).toBeNull();
    expect(screen.queryByRole("button", { name: "Add for a user" })).toBeNull();
  });

  it("disables every change without bridges:write", async () => {
    signOut();
    await signIn(["bridges:read"] as Scope[]);
    renderOffering("mautrix-whatsapp");
    expect(await screen.findByRole("button", { name: "Add for a user" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Edit settings" })).toBeDisabled();
    expect(await tableButton("Files for @alice:example.org")).toBeDisabled();
  });

  it("says so honestly when the server has no offerings API yet", async () => {
    server.use(
      http.get("/api/v1/bridge-offerings/:type", () =>
        HttpResponse.json(
          { type: "urn:hs:problem:not-implemented", title: "Not implemented", status: 501 },
          { status: 501 },
        ),
      ),
    );
    renderOffering("mautrix-whatsapp");
    expect(await screen.findByText(/isn't implemented on this server yet/)).toBeInTheDocument();
  });
});
