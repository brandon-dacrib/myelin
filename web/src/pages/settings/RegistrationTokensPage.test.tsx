import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { RegistrationTokensPage } from "./RegistrationTokensPage";
import { renderSettingsRoute } from "./test-utils";
import { findRegistrationToken } from "@/mocks/data/registration-tokens";
import { signIn, signOut } from "@/lib/auth";

const PATH = "/settings/registration-tokens";

async function table() {
  return within(await screen.findByRole("table", { name: "Invite links" }));
}

beforeEach(async () => {
  await signIn();
});

afterEach(() => {
  signOut();
});

describe("RegistrationTokensPage", () => {
  it("says whether each token works and, when it does not, why", async () => {
    renderSettingsRoute(PATH, RegistrationTokensPage);
    const rows = await table();

    const row = (token: string) => within(rows.getByRole("row", { name: new RegExp(token) }));
    expect(row("welcome-team").getByText("Valid")).toBeInTheDocument();
    expect(row("welcome-team").getByText("4 of unlimited")).toBeInTheDocument();
    expect(row("welcome-team").getAllByText("Never").length).toBeGreaterThan(0);

    expect(row("carol-invite").getByText("Valid")).toBeInTheDocument();
    expect(row("carol-invite").getByText("0 of 1")).toBeInTheDocument();
    expect(row("carol-invite").getByText(/^in \d+ days$/)).toBeInTheDocument();

    expect(row("spring-cohort").getByText("Expired")).toBeInTheDocument();
    expect(row("spring-cohort").getByText("3 of 10")).toBeInTheDocument();
    expect(row("spring-cohort").getByText("2 days ago")).toBeInTheDocument();

    expect(row("dave-invite").getByText("Used up")).toBeInTheDocument();
    expect(row("erin-invite").getByText("Uses in progress")).toBeInTheDocument();
    expect(
      row("erin-invite").getByText("1 registration is still finishing with it."),
    ).toBeInTheDocument();
  });

  it("copies a row's invite link", async () => {
    const user = userEvent.setup();
    renderSettingsRoute(PATH, RegistrationTokensPage);
    const rows = await table();

    await user.click(rows.getByRole("button", { name: "Copy invite link for carol-invite" }));
    await expect(navigator.clipboard.readText()).resolves.toBe(
      `${window.location.origin}/admin/register?token=carol-invite`,
    );
  });

  it("creates a token from the page and lists it", async () => {
    const user = userEvent.setup();
    renderSettingsRoute(PATH, RegistrationTokensPage);
    await table();

    await user.click(screen.getByRole("button", { name: "Create invite link" }));
    const dialog = within(await screen.findByRole("dialog", { name: "Create an invite link" }));
    await user.click(dialog.getByRole("radio", { name: "Choose my own" }));
    await user.type(dialog.getByLabelText(/^Custom token/), "frank-invite");
    await user.click(dialog.getByRole("button", { name: "Create invite link" }));
    const done = within(await screen.findByRole("dialog", { name: "Invite link ready" }));
    await user.click(done.getByRole("button", { name: "Done" }));

    const rows = await table();
    expect(await rows.findByRole("row", { name: /frank-invite/ })).toBeInTheDocument();
  });

  it("changes a token's uses and expiry, and expires it on the spot", async () => {
    const user = userEvent.setup();
    renderSettingsRoute(PATH, RegistrationTokensPage);
    const rows = await table();

    await user.click(rows.getByRole("button", { name: "Edit welcome-team" }));
    let dialog = within(await screen.findByRole("dialog", { name: "Edit welcome-team" }));
    expect(dialog.getByRole("switch", { name: "Unlimited" })).toBeChecked();
    expect(dialog.getByRole("radio", { name: "Never" })).toBeChecked();
    await user.click(dialog.getByRole("switch", { name: "Unlimited" }));
    await user.clear(dialog.getByLabelText(/^Uses allowed/));
    await user.type(dialog.getByLabelText(/^Uses allowed/), "10");
    await user.click(dialog.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(findRegistrationToken("welcome-team")?.uses_allowed).toBe(10));
    expect(await rows.findByText("4 of 10")).toBeInTheDocument();

    await user.click(rows.getByRole("button", { name: "Edit carol-invite" }));
    dialog = within(await screen.findByRole("dialog", { name: "Edit carol-invite" }));
    expect(dialog.getByRole("radio", { name: "Date and time" })).toBeChecked();
    await user.click(dialog.getByRole("button", { name: "Expire now" }));

    await waitFor(() => expect(findRegistrationToken("carol-invite")?.valid).toBe(false));
    const carol = within(await rows.findByRole("row", { name: /carol-invite/ }));
    expect(await carol.findByText("Expired")).toBeInTheDocument();
  });

  it("deletes a token only once the operator confirms", async () => {
    const user = userEvent.setup();
    renderSettingsRoute(PATH, RegistrationTokensPage);
    const rows = await table();

    await user.click(rows.getByRole("button", { name: "Delete dave-invite" }));
    let dialog = within(await screen.findByRole("dialog", { name: "Delete dave-invite?" }));
    expect(dialog.getByText(/Accounts already created stay/)).toBeInTheDocument();
    await user.click(dialog.getByRole("button", { name: "Cancel" }));
    expect(findRegistrationToken("dave-invite")).toBeDefined();

    await user.click(rows.getByRole("button", { name: "Delete dave-invite" }));
    dialog = within(await screen.findByRole("dialog", { name: "Delete dave-invite?" }));
    await user.click(dialog.getByRole("button", { name: "Delete token" }));
    await waitFor(() => expect(findRegistrationToken("dave-invite")).toBeUndefined());
    await waitFor(() =>
      expect(rows.queryByRole("row", { name: /dave-invite/ })).not.toBeInTheDocument(),
    );
  });

  it("offers no changes to a read-only operator", async () => {
    signOut();
    await signIn(["admin:read"]);
    renderSettingsRoute(PATH, RegistrationTokensPage);
    const rows = await table();

    expect(rows.getByRole("button", { name: "Copy invite link for carol-invite" })).toBeVisible();
    expect(rows.queryByRole("button", { name: /^Edit / })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Create invite link" })).not.toBeInTheDocument();
  });
});
