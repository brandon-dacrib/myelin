import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { ErrorState, ForbiddenState, NotImplementedState } from "./ErrorState";

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

describe("NotImplementedState", () => {
  it("says not implemented, as status not alert (it isn't a fault)", () => {
    render(<NotImplementedState resource="Bridges" />);
    expect(screen.getByRole("status")).toHaveTextContent("isn't implemented on this server yet");
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });

  it("says unavailable, with a distinct message, for the 503 variant", () => {
    render(<NotImplementedState resource="Users" variant="unavailable" />);
    expect(screen.getByRole("status")).toHaveTextContent(
      "isn't connected to a data source on this server yet",
    );
  });

  it("offers Check again only when onRetry is given", async () => {
    const onRetry = vi.fn();
    const { rerender } = render(<NotImplementedState resource="Bridges" />);
    expect(screen.queryByRole("button", { name: "Check again" })).not.toBeInTheDocument();

    rerender(<NotImplementedState resource="Bridges" onRetry={onRetry} />);
    await userEvent.click(screen.getByRole("button", { name: "Check again" }));
    expect(onRetry).toHaveBeenCalledTimes(1);
  });

  it("shows the server's own detail message when given one", () => {
    render(
      <NotImplementedState
        resource="Bridges"
        problem={{ detail: "Bridges are not wired up yet." }}
      />,
    );
    expect(screen.getByText("Bridges are not wired up yet.")).toBeInTheDocument();
  });
});
