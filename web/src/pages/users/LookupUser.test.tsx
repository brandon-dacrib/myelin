import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { signIn, signOut } from "@/lib/auth";
import { renderRoutes } from "@/test/render-route";
import { LookupUser } from "./LookupUser";

function open() {
  return renderRoutes(
    [
      { path: "/users", component: LookupUser },
      { path: "/users/$userId", component: () => <p>user page</p> },
    ],
    "/users",
  );
}

beforeEach(async () => {
  await signIn();
});
afterEach(() => signOut());

describe("Find by email, phone or sign-in provider", () => {
  it("opens the account that has exactly this email", async () => {
    const user = userEvent.setup();
    const { router } = open();
    await user.click(await screen.findByText("Find by email, phone or sign-in provider"));
    expect(screen.getByText(/one account that has exactly this/)).toBeInTheDocument();
    await user.type(screen.getByLabelText(/^Email address/), "Alice@Example.org");
    await user.click(screen.getByRole("button", { name: "Find the account" }));
    expect(await screen.findByText("user page")).toBeInTheDocument();
    expect(router.state.location.pathname).toBe("/users/%40alice%3Aexample.org");
  });

  it("says when nobody has that phone number, and asks for the number first", async () => {
    const user = userEvent.setup();
    open();
    await user.click(await screen.findByText("Find by email, phone or sign-in provider"));
    await user.click(screen.getByRole("combobox", { name: /what you have/i }));
    await user.click(await screen.findByRole("option", { name: "Phone number" }));
    await user.click(screen.getByRole("button", { name: "Find the account" }));
    expect(await screen.findByRole("status")).toHaveTextContent(
      "Give the phone number to look up.",
    );
    await user.type(screen.getByLabelText(/^Phone number/), "15551234567");
    await user.click(screen.getByRole("button", { name: "Find the account" }));
    expect(await screen.findByRole("status")).toHaveTextContent(
      "No account has 15551234567 as a verified phone number.",
    );
  });

  it("finds the account a sign-in provider knows by a subject", async () => {
    const user = userEvent.setup();
    const { router } = open();
    await user.click(await screen.findByText("Find by email, phone or sign-in provider"));
    await user.click(screen.getByRole("combobox", { name: /what you have/i }));
    await user.click(await screen.findByRole("option", { name: "Sign-in provider subject" }));
    await user.type(screen.getByLabelText(/^Provider/), "oidc-corp");
    await user.type(screen.getByLabelText(/^Subject/), "248289761001");
    await user.click(screen.getByRole("button", { name: "Find the account" }));
    expect(await screen.findByText("user page")).toBeInTheDocument();
    expect(router.state.location.pathname).toBe("/users/%40alice%3Aexample.org");
  });
});
