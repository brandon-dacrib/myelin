import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { putOffering } from "@/mocks/data/bridge-offerings";
import { signIn, signOut, type Scope } from "@/lib/auth";
import { renderBridgesRoute } from "../test-utils";
import { BridgeOfferingPage } from "./BridgeOfferingPage";

/**
 * One Hookshot, many webhooks: the offering's page adds a webhook from a source, in a room the
 * bot is in or a new one, shows the URL once with the sender's setup, and removes a connection
 * after confirmation. Against the mock API (`src/mocks/data/bridge-offerings.ts`), which makes
 * connections the way the server does, minus the bridge.
 */

function renderHookshot() {
  putOffering("matrix-hookshot", {
    runtime: "cluster",
    enabled: true,
    options: { public_url: "https://hooks.example.org" },
  });
  return renderBridgesRoute(
    "/bridges/offerings/$type",
    BridgeOfferingPage,
    "/bridges/offerings/matrix-hookshot",
  );
}

async function connectionsTable() {
  const section = await screen.findByTestId("connections");
  return within(section).findByRole("table");
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("Add a webhook", () => {
  it("makes a webhook from a source in a new room and shows the URL once with the setup", async () => {
    const user = userEvent.setup();
    renderHookshot();
    await connectionsTable();
    await user.click(screen.getByRole("button", { name: "Add a webhook" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a webhook" });
    // The sources, as cards; the plain one is chosen until another is.
    const sources = await within(dialog).findByRole("radiogroup", { name: "What will post to it" });
    expect(within(sources).getByRole("radio", { name: /Plain JSON/ })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    await user.click(within(sources).getByRole("radio", { name: /Datadog monitors/ }));
    // A new room, with someone invited.
    await user.click(within(dialog).getByRole("radio", { name: /A new room/ }));
    await user.type(within(dialog).getByLabelText(/Room name/), "Alerts");
    await user.type(within(dialog).getByLabelText(/Invite/), "@alice:example.org");
    await user.type(within(dialog).getByLabelText(/^Name/), "datadog-alerts");
    await user.click(within(dialog).getByRole("button", { name: "Connect" }));

    const made = await screen.findByRole("dialog", { name: "Webhook datadog-alerts is connected" });
    expect(made).toHaveTextContent("it will not be shown again");
    const url = within(made).getByTestId("webhook-url");
    expect(url).toHaveTextContent(/^https:\/\/hooks\.example\.org\/webhook\/[0-9a-f-]{36}$/);
    expect(within(url).getByRole("button", { name: "Copy webhook URL" })).toBeInTheDocument();
    // The sender's setup, with the URL filled in and rendered as text, not Markdown.
    const setup = within(made).getByTestId("webhook-setup");
    expect(setup).toHaveTextContent("Integrations → Webhooks → New Webhook");
    expect(setup).toHaveTextContent("@webhook-matrix-datadog-alerts");
    expect(setup.textContent).not.toContain("**");
    expect(setup.textContent).not.toContain("{{url}}");
    await user.click(within(made).getByRole("button", { name: "Done" }));

    // Listed, with its source and its new room; the URL is nowhere on the page.
    const table = await connectionsTable();
    await waitFor(() => expect(within(table).getByText("datadog-alerts")).toBeInTheDocument());
    const row = within(table).getByText("datadog-alerts").closest("tr") as HTMLElement;
    expect(row).toHaveTextContent("Source: datadog");
    expect(row).toHaveTextContent("Alerts");
    expect(document.body.textContent).not.toContain("/webhook/");
  });

  it("gives a Gmail webhook its Apps Script with the URL in it", async () => {
    const user = userEvent.setup();
    renderHookshot();
    await connectionsTable();
    await user.click(screen.getByRole("button", { name: "Add a webhook" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a webhook" });
    await user.click(await within(dialog).findByRole("radio", { name: /Gmail/ }));
    // The room the bot is in with nothing connected yet is offered.
    expect(within(dialog).getByRole("radio", { name: /A room the bot is in/ })).toHaveAttribute(
      "aria-checked",
      "true",
    );
    const roomSelect = within(dialog).getByRole("combobox");
    roomSelect.focus();
    await user.keyboard("{Enter}");
    await user.click(await screen.findByRole("option", { name: /Ops/ }));
    await user.type(within(dialog).getByLabelText(/^Name/), "mail");
    await user.click(within(dialog).getByRole("button", { name: "Connect" }));
    const made = await screen.findByRole("dialog", { name: "Webhook mail is connected" });
    expect(within(made).getByRole("button", { name: "Copy Code.gs" })).toBeInTheDocument();
    expect(made).toHaveTextContent(/const WEBHOOK_URL = "https:\/\/hooks\.example\.org\/webhook\//);
  });

  it("says what is wrong at the field, before and after asking the server", async () => {
    const user = userEvent.setup();
    renderHookshot();
    await connectionsTable();
    await user.click(screen.getByRole("button", { name: "Add a webhook" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a webhook" });
    await within(dialog).findByRole("radiogroup", { name: "What will post to it" });
    await user.type(within(dialog).getByLabelText(/^Name/), "ab");
    await user.click(within(dialog).getByRole("button", { name: "Connect" }));
    expect(await within(dialog).findByText("3 to 64 characters.")).toBeInTheDocument();
    // The same name as a webhook the room already has: the server's 409, shown.
    const roomSelect = within(dialog).getByRole("combobox");
    roomSelect.focus();
    await user.keyboard("{Enter}");
    await user.click(await screen.findByRole("option", { name: /myelin pull requests/ }));
    await user.clear(within(dialog).getByLabelText(/^Name/));
    await user.type(within(dialog).getByLabelText(/^Name/), "myelin");
    await user.click(within(dialog).getByRole("button", { name: "Connect" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      "already has a webhook named `myelin`",
    );
  });

  it("removes a connection after confirmation, and the row says so", async () => {
    const user = userEvent.setup();
    renderHookshot();
    const table = await connectionsTable();
    await user.click(within(table).getByRole("button", { name: "Remove brandon-dacrib/myelin" }));
    const dialog = await screen.findByRole("dialog", { name: "Remove brandon-dacrib/myelin?" });
    expect(dialog).toHaveTextContent("Nothing more from it lands in myelin pull requests");
    await user.click(within(dialog).getByRole("button", { name: "Remove" }));
    await waitFor(() =>
      expect(
        within(table).queryByRole("button", { name: "Remove brandon-dacrib/myelin" }),
      ).toBeNull(),
    );
    expect(within(table).getAllByText("Removed")).toHaveLength(2);
    // A webhook's dialog says its URL stops working.
    await user.click(within(table).getByRole("button", { name: "Remove myelin" }));
    expect(await screen.findByRole("dialog", { name: "Remove myelin?" })).toHaveTextContent(
      "Its URL stops working",
    );
  });

  it("needs bridges:write to add or remove", async () => {
    signOut();
    await signIn(["bridges:read"] as Scope[]);
    renderHookshot();
    const table = await connectionsTable();
    expect(screen.getByRole("button", { name: "Add a webhook" })).toBeDisabled();
    expect(within(table).getByRole("button", { name: "Remove myelin" })).toBeDisabled();
  });
});
