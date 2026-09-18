import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Button } from "./Button";

describe("Button", () => {
  it("renders children and responds to clicks", async () => {
    const onClick = vi.fn();
    render(<Button onClick={onClick}>Add bridge</Button>);
    const button = screen.getByRole("button", { name: "Add bridge" });
    await userEvent.click(button);
    expect(onClick).toHaveBeenCalledTimes(1);
  });

  it("does not fire onClick when disabled", async () => {
    const onClick = vi.fn();
    render(
      <Button onClick={onClick} disabled>
        Add bridge
      </Button>,
    );
    await userEvent.click(screen.getByRole("button", { name: "Add bridge" }));
    expect(onClick).not.toHaveBeenCalled();
  });

  it("uses aria-label for icon-only buttons instead of visible text", () => {
    render(
      <Button size="icon" aria-label="Delete">
        <span aria-hidden="true">x</span>
      </Button>,
    );
    expect(screen.getByRole("button", { name: "Delete" })).toBeInTheDocument();
  });
});
