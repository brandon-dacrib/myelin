import { afterEach, describe, expect, it } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { IntegrationsApp } from "./IntegrationsApp";
import { forgetSession, getUserSession } from "./auth";

/**
 * The self-service page (decision 0057) against the mock user API
 * (`src/mocks/data/user-webhooks.ts`): sign in with an ordinary account, see the webhooks in
 * my rooms, add one in a room of mine (only the rooms I may set up are offered; the others say
 * why not), read the URL once with the sender's setup, and remove one after confirming.
 */

function renderApp() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={client}>
      <IntegrationsApp />
    </QueryClientProvider>,
  );
}

async function signInAsAlice(user: ReturnType<typeof userEvent.setup>) {
  await user.type(screen.getByLabelText(/Username/), "alice");
  await user.type(screen.getByLabelText(/Password/), "a-password");
  await user.click(screen.getByRole("button", { name: "Sign in" }));
  await screen.findByRole("heading", { name: "My webhooks" });
}

afterEach(() => forgetSession());

describe("The integrations page", () => {
  it("signs in with a Matrix account and lists the webhooks in my rooms", async () => {
    const user = userEvent.setup();
    renderApp();
    expect(screen.getByRole("heading", { name: "Integrations" })).toBeInTheDocument();
    // A wrong password is said plainly (the mock refuses `wrong`).
    await user.type(screen.getByLabelText(/Username/), "alice");
    await user.type(screen.getByLabelText(/Password/), "wrong");
    await user.click(screen.getByRole("button", { name: "Sign in" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "That username and password were not accepted",
    );
    await user.clear(screen.getByLabelText(/Password/));
    await user.type(screen.getByLabelText(/Password/), "a-password");
    await user.click(screen.getByRole("button", { name: "Sign in" }));
    await screen.findByRole("heading", { name: "My webhooks" });
    expect(getUserSession()?.userId).toBe("@alice:example.org");
    expect(screen.getByTestId("signed-in-as")).toHaveTextContent("@alice:example.org");
    const list = await screen.findByTestId("webhook-list");
    expect(list).toHaveTextContent("uptime");
    expect(list).toHaveTextContent("Ops");
    expect(list).toHaveTextContent("made by @alice:example.org");
    // Nothing on the page is a URL with a secret in it.
    expect(document.body.textContent).not.toContain("/webhook/");
  });

  it("adds a webhook in one of my rooms, offering only the rooms I may set up", async () => {
    const user = userEvent.setup();
    renderApp();
    await signInAsAlice(user);
    await user.click(screen.getByRole("button", { name: "Add a webhook" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a webhook" });
    const sources = await within(dialog).findByRole("radiogroup", { name: "What will post to it" });
    await user.click(within(sources).getByRole("radio", { name: /Datadog monitors/ }));
    // Two of four rooms can take one; the two that cannot say why.
    await within(dialog).findByText(/2 of your 4 rooms can take one/);
    await user.click(within(dialog).getByText("2 of your rooms cannot take a webhook"));
    expect(dialog).toHaveTextContent("Team chat: adding a webhook there takes power level 50");
    expect(dialog).toHaveTextContent("Secret: that room is encrypted");
    await user.click(within(dialog).getByRole("combobox", { name: /Room/ }));
    const listbox = await screen.findByRole("listbox");
    expect(within(listbox).queryByRole("option", { name: "Team chat" })).not.toBeInTheDocument();
    await user.click(within(listbox).getByRole("option", { name: "Deploys" }));
    await user.type(within(dialog).getByLabelText(/^Name/), "datadog-alerts");
    await user.click(within(dialog).getByRole("button", { name: "Connect" }));

    const made = await screen.findByRole("dialog", { name: "Webhook datadog-alerts is connected" });
    expect(made).toHaveTextContent("it will not be shown again");
    const url = within(made).getByTestId("webhook-url");
    expect(url).toHaveTextContent(/^https:\/\/hooks\.example\.org\/webhook\/[0-9a-f-]{36}$/);
    expect(within(url).getByRole("button", { name: "Copy webhook URL" })).toBeInTheDocument();
    const setup = within(made).getByTestId("webhook-setup");
    expect(setup).toHaveTextContent("@webhook-matrix-datadog-alerts");
    expect(setup.textContent).not.toContain("{{url}}");
    await user.click(within(made).getByRole("button", { name: "Done" }));
    await waitFor(() => {
      expect(screen.getByTestId("webhook-list")).toHaveTextContent("datadog-alerts");
    });
    expect(screen.getByTestId("webhook-list")).toHaveTextContent("Deploys");
    expect(screen.getByTestId("webhook-list")).not.toHaveTextContent("/webhook/");
  });

  it("makes a new room, carries the Gmail script, and says what the server refused", async () => {
    const user = userEvent.setup();
    renderApp();
    await signInAsAlice(user);
    await user.click(screen.getByRole("button", { name: "Add a webhook" }));
    const dialog = await screen.findByRole("dialog", { name: "Add a webhook" });
    const sources = await within(dialog).findByRole("radiogroup", { name: "What will post to it" });
    await user.click(within(sources).getByRole("radio", { name: /Gmail/ }));
    await user.click(within(dialog).getByRole("radio", { name: /A new room/ }));
    await user.type(within(dialog).getByLabelText(/Room name/), "Mail");
    // A name too short is refused before the server sees it.
    await user.type(within(dialog).getByLabelText(/^Name/), "ab");
    await user.click(within(dialog).getByRole("button", { name: "Connect" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("3 to 64 characters");
    await user.clear(within(dialog).getByLabelText(/^Name/));
    await user.type(within(dialog).getByLabelText(/^Name/), "inbox");
    await user.click(within(dialog).getByRole("button", { name: "Connect" }));
    const made = await screen.findByRole("dialog", { name: "Webhook inbox is connected" });
    expect(within(made).getByRole("button", { name: "Copy Code.gs" })).toBeInTheDocument();
    expect(made).toHaveTextContent("const WEBHOOK_URL");
    await user.click(within(made).getByRole("button", { name: "Done" }));
    await waitFor(() => {
      expect(screen.getByTestId("webhook-list")).toHaveTextContent("Mail");
    });

    // The same name in the same room again: the server's refusal, shown as it was said.
    await user.click(screen.getByRole("button", { name: "Add a webhook" }));
    const again = await screen.findByRole("dialog", { name: "Add a webhook" });
    await within(again).findByText(/of your .* rooms can take one/);
    await user.click(within(again).getByRole("combobox", { name: /Room/ }));
    await user.click(await screen.findByRole("option", { name: "Ops" }));
    await user.type(within(again).getByLabelText(/^Name/), "uptime");
    await user.click(within(again).getByRole("button", { name: "Connect" }));
    expect(await within(again).findByRole("alert")).toHaveTextContent(
      "the room already has a webhook named `uptime`",
    );
  });

  it("removes a webhook after confirming, and signs out", async () => {
    const user = userEvent.setup();
    renderApp();
    await signInAsAlice(user);
    await screen.findByTestId("webhook-list");
    await user.click(screen.getByRole("button", { name: "Remove uptime" }));
    const confirm = await screen.findByRole("dialog", { name: "Remove uptime?" });
    expect(confirm).toHaveTextContent(
      "Its URL stops working and nothing more from it lands in Ops",
    );
    await user.click(within(confirm).getByRole("button", { name: "Remove" }));
    await waitFor(() => {
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    });
    expect(await screen.findByText("No webhooks in your rooms yet")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Sign out" }));
    await screen.findByRole("button", { name: "Sign in" });
    expect(getUserSession()).toBeNull();
  });
});
