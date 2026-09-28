import { afterEach, describe, expect, it } from "vitest";
import { screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { findUser } from "@/mocks/data/users";
import { UsersPage } from "../UsersPage";

/** The users list's "Suspended only" switch, and the status badges each row carries. */
const ROUTES = [
  {
    path: "/users",
    component: UsersPage,
    validateSearch: (s: Record<string, unknown>) => ({
      q: typeof s.q === "string" ? s.q : undefined,
      cursor: typeof s.cursor === "string" ? s.cursor : undefined,
      suspended: s.suspended === true || s.suspended === "true" ? true : undefined,
    }),
  },
];
const KNOWN = ["/users/$userId"];

afterEach(() => signOut());

describe("Users list: suspended and shadow-banned", { timeout: 20_000 }, () => {
  it("shows each user's moderation state and narrows to the suspended", async () => {
    await signIn();
    findUser("@alice:example.org")!.shadow_banned = true;
    const user = userEvent.setup();
    renderRoutes(ROUTES, "/users", KNOWN);

    const table = within(await screen.findByRole("table", { name: "Users" }, { timeout: 5000 }));
    const alice = (await table.findByRole("link", { name: "@alice:example.org" })).closest("tr")!;
    expect(within(alice).getByText("Shadow-banned")).toBeInTheDocument();
    const spammer = table.getByRole("link", { name: "@spammer42:example.org" }).closest("tr")!;
    expect(within(spammer).getByText("Suspended")).toBeInTheDocument();

    await user.click(screen.getByRole("switch", { name: "Suspended only" }));
    // The table is drawn again for the new search, so look it up afresh.
    const rows = () => within(screen.getByRole("table", { name: "Users" }));
    await expect.poll(() => rows().queryByRole("link", { name: "@alice:example.org" })).toBeNull();
    expect(rows().getByRole("link", { name: "@spammer42:example.org" })).toBeInTheDocument();
  });
});
