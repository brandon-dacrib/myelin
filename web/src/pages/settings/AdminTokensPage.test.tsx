import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { AdminTokensPage } from "./AdminTokensPage";
import { renderSettingsRoute } from "./test-utils";
import { adminTokens } from "@/mocks/data/admin-tokens";
import { signIn, signOut } from "@/lib/auth";

const PATH = "/settings/admin-tokens";

async function table() {
  return within(await screen.findByRole("table", { name: "Admin tokens" }));
}

beforeEach(async () => {
  await signIn();
});

afterEach(() => {
  signOut();
});

describe("AdminTokensPage", () => {
  it("lists each token with the scopes it carries and explains the page", async () => {
    renderSettingsRoute(PATH, AdminTokensPage);
    const rows = await table();
    const row = (name: string) => within(rows.getByRole("row", { name: new RegExp(name) }));

    // A full administrator's token is one badge, since admin:write includes everything.
    expect(row("Deploy pipeline").getByText("admin:write")).toBeInTheDocument();
    expect(row("Deploy pipeline").queryByText("admin:read")).not.toBeInTheDocument();
    expect(row("Bridge team dashboard").getByText("bridges:read")).toBeInTheDocument();
    expect(row("Bridge team dashboard").getByText(/^in \d+ days$/)).toBeInTheDocument();
    expect(row("Moderators' bot").getByText("moderation:write")).toBeInTheDocument();
    expect(row("Moderators' bot").getByText("Never")).toBeInTheDocument();
    // The badge carries the sentence for the scope.
    expect(
      row("Bridge team dashboard").getByText("bridges:read").closest("span[title]"),
    ).toHaveAttribute("title", expect.stringMatching(/See the bridges/));
    expect(
      screen.getByText(/A request outside its scopes is refused, and the refusal names the scope/),
    ).toBeInTheDocument();
  });

  it("mints a token with chosen scopes, shows it once, and lists it", async () => {
    const user = userEvent.setup();
    renderSettingsRoute(PATH, AdminTokensPage);
    await table();

    await user.click(screen.getByRole("button", { name: "Mint token" }));
    const dialog = within(await screen.findByRole("dialog", { name: "Mint an admin token" }));
    // Every scope is explained in a sentence.
    expect(
      dialog.getByText(/Add, change, pause, resume, replay and remove bridges/),
    ).toBeInTheDocument();
    expect(dialog.getByText(/Suspend, lock, shadow-ban and sign out users/)).toBeInTheDocument();
    // It starts as a full administrator's token.
    expect(dialog.getByRole("checkbox", { name: /^admin:write/ })).toBeChecked();
    expect(dialog.getByText(/A full administrator’s token/)).toBeInTheDocument();

    await user.type(dialog.getByLabelText(/^Name/), "Bridge bot");
    await user.click(dialog.getByRole("checkbox", { name: /^admin:write/ }));
    await user.click(dialog.getByRole("checkbox", { name: /^admin:read/ }));
    await user.click(dialog.getByRole("checkbox", { name: /^bridges:write/ }));
    expect(dialog.getByText("The token will hold bridges:write.")).toBeInTheDocument();
    await user.click(dialog.getByRole("checkbox", { name: /^bridges:read/ }));
    expect(dialog.getByText(/already included/)).toBeInTheDocument();
    await user.click(dialog.getByRole("radio", { name: "30 days" }));
    await user.click(dialog.getByRole("button", { name: "Mint token" }));

    const done = within(await screen.findByRole("dialog", { name: "Admin token ready" }));
    expect(done.getByTestId("token").textContent).toMatch(/^hsa_/);
    expect(done.getByText("bridges:read, bridges:write")).toBeInTheDocument();
    expect(done.getByText(/^in \d+ days$/)).toBeInTheDocument();
    await user.click(done.getByRole("button", { name: "Done" }));

    const rows = await table();
    await waitFor(() => expect(rows.getByRole("row", { name: /Bridge bot/ })).toBeInTheDocument());
    const row = within(rows.getByRole("row", { name: /Bridge bot/ }));
    expect(row.getByText("bridges:write")).toBeInTheDocument();
    expect(adminTokens.find((t) => t.name === "Bridge bot")?.scopes).toEqual([
      "bridges:read",
      "bridges:write",
    ]);
  });

  it("refuses to mint a token with no scopes, and says so beside the picker", async () => {
    const user = userEvent.setup();
    renderSettingsRoute(PATH, AdminTokensPage);
    await table();
    await user.click(screen.getByRole("button", { name: "Mint token" }));
    const dialog = within(await screen.findByRole("dialog", { name: "Mint an admin token" }));
    await user.type(dialog.getByLabelText(/^Name/), "Nothing");
    await user.click(dialog.getByRole("checkbox", { name: /^admin:write/ }));
    await user.click(dialog.getByRole("checkbox", { name: /^admin:read/ }));
    expect(dialog.getByText("No scopes: the token could do nothing.")).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Mint token" }));
    expect(dialog.getByRole("alert")).toHaveTextContent("Choose at least one scope.");
  });

  it("revokes a token after confirming", async () => {
    const user = userEvent.setup();
    renderSettingsRoute(PATH, AdminTokensPage);
    const rows = await table();
    await user.click(rows.getByRole("button", { name: "Revoke Bridge team dashboard" }));
    const confirm = within(
      await screen.findByRole("dialog", { name: "Revoke Bridge team dashboard?" }),
    );
    expect(confirm.getByText(/stops working at its next request/)).toBeInTheDocument();
    await user.click(confirm.getByRole("button", { name: "Revoke token" }));
    await waitFor(() =>
      expect(screen.queryByRole("row", { name: /Bridge team dashboard/ })).not.toBeInTheDocument(),
    );
    expect(adminTokens.some((t) => t.name === "Bridge team dashboard")).toBe(false);
  });
});
