import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { UserDetailPage } from "./UserDetailPage";

const KNOWN = ["/", "/rooms/$roomId", "/audit", "/bridges/$bridgeId", "/users"];

function open(userId: string) {
  return renderRoutes(
    [{ path: "/users/$userId", component: UserDetailPage }],
    `/users/${encodeURIComponent(userId)}`,
    KNOWN,
  );
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("A user's page", () => {
  it("names the kind of account and the bridge that made it, in words", async () => {
    open("@whatsapp_15551234:example.org");
    expect(await screen.findByText("Kind of account")).toBeInTheDocument();
    expect(screen.getByText("Made by a bridge")).toBeInTheDocument();
  });

  it("deactivates an account, says what that does, and reactivates it", async () => {
    const user = userEvent.setup();
    open("@spammer42:example.org");
    const danger = await screen.findByText("Deactivate this user");
    expect(danger.parentElement).toHaveTextContent(/signed out everywhere/);

    await user.click(screen.getByRole("button", { name: "Deactivate" }));
    const dialog = within(await screen.findByRole("dialog"));
    expect(dialog.getByText(/Reactivate on this page lets them sign in again/)).toBeVisible();
    await user.click(dialog.getByRole("button", { name: "Deactivate" }));

    const reactivate = await screen.findByRole("button", { name: "Reactivate" });
    expect(screen.getByText(/Rooms they were taken out of/)).toBeInTheDocument();
    await user.click(reactivate);
    expect(await screen.findByRole("button", { name: "Deactivate" })).toBeVisible();
    expect(screen.queryByRole("button", { name: "Reactivate" })).not.toBeInTheDocument();
  });
});
