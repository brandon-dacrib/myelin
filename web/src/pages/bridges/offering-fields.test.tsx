import { useState } from "react";
import { describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { AccessFields } from "./offering-fields";

function Harness({ initial = [] as string[] }) {
  const [state, setState] = useState({ allLocalUsers: false, users: initial });
  return (
    <div>
      <AccessFields
        allLocalUsers={state.allLocalUsers}
        users={state.users}
        serverName="example.org"
        onChange={(patch) => setState((s) => ({ ...s, ...patch }))}
      />
      <output data-testid="users">{state.users.join("|")}</output>
    </div>
  );
}

const users = () => screen.getByTestId("users").textContent;

describe("the list of people who can have a bridge", () => {
  it("is a list of IDs, one row each, not a box of text", () => {
    const { container } = render(<Harness initial={["@alice:example.org"]} />);
    const list = screen.getByRole("list", { name: "People listed" });
    expect(within(list).getByText("@alice:example.org")).toBeInTheDocument();
    expect(container.querySelector("textarea")).toBeNull();
  });

  it("adds an ID with Enter, and several pasted at once", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    const input = screen.getByLabelText("People who can have one");

    await user.type(input, "@alice:example.org{Enter}");
    expect(users()).toBe("@alice:example.org");
    expect(input).toHaveValue("");

    await user.type(input, "@bob:example.org, @ops:example.org @alice:example.org{Enter}");
    expect(users()).toBe("@alice:example.org|@bob:example.org|@ops:example.org");
  });

  it("keeps something that is not a Matrix ID in the box, saying why", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    const input = screen.getByLabelText("People who can have one");

    await user.type(input, "alice{Enter}");
    expect(users()).toBe("");
    expect(input).toHaveValue("alice");
    expect(screen.getByRole("alert")).toHaveTextContent("Not a Matrix ID: alice");
    expect(input).toHaveAttribute("aria-invalid", "true");
    expect(screen.getByRole("button", { name: "Add" })).toBeDisabled();
  });

  it("adds what was typed when focus leaves the box, so it is not lost on save", async () => {
    const user = userEvent.setup();
    render(<Harness />);
    await user.type(screen.getByLabelText("People who can have one"), "@carol:example.org");
    await user.tab();
    expect(users()).toBe("@carol:example.org");
  });

  it("removes one person, and says the list may not be empty", async () => {
    const user = userEvent.setup();
    render(<Harness initial={["@alice:example.org"]} />);
    await user.click(screen.getByRole("button", { name: "Remove @alice:example.org" }));
    expect(users()).toBe("");
    expect(screen.getByRole("alert")).toHaveTextContent("List at least one person");
  });
});
