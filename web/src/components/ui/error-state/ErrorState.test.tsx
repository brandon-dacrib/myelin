import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ErrorState, ForbiddenState } from "./ErrorState";

describe("ErrorState", () => {
  it("shows the plain-language title and detail, and calls onRetry", async () => {
    const onRetry = vi.fn();
    render(
      <ErrorState
        title="Couldn't load bridges"
        problem={{ detail: "The server returned a 503.", requestId: "req_1" }}
        onRetry={onRetry}
      />,
    );
    expect(screen.getByRole("alert")).toHaveTextContent("Couldn't load bridges");
    expect(screen.getByText("The server returned a 503.")).toBeInTheDocument();
    expect(screen.getByText(/req_1/)).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: "Retry" }));
    expect(onRetry).toHaveBeenCalledTimes(1);
  });

  it("falls back to a generic title when none is given", () => {
    render(<ErrorState />);
    expect(screen.getByText("Something went wrong")).toBeInTheDocument();
  });
});

describe("ForbiddenState", () => {
  it("names the missing scope, never a generic error", () => {
    render(<ForbiddenState scope="bridges:read" />);
    expect(screen.getByText("bridges:read")).toBeInTheDocument();
    expect(screen.getByRole("alert")).toHaveTextContent("Ask an administrator to grant it.");
  });
});
