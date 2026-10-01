import { describe, expect, it } from "vitest";
import { render, screen, within } from "@testing-library/react";
import { AppliesBadge, AppliesLegend } from "./AppliesBadge";
import { RateLimitsNote } from "./SectionNotes";

describe("AppliesBadge", () => {
  it("names each class in words", () => {
    render(
      <>
        <AppliesBadge applies="hot" />
        <AppliesBadge applies="restart" />
        <AppliesBadge applies="bootstrap" />
      </>,
    );
    expect(screen.getByText("Applies on save")).toBeInTheDocument();
    expect(screen.getByText("Needs a restart")).toBeInTheDocument();
    expect(screen.getByText("Per replica (file or environment)")).toBeInTheDocument();
  });
});

describe("AppliesLegend", () => {
  it("explains each class the section has, once, with how many settings are in it", () => {
    render(<AppliesLegend counts={{ hot: 9, restart: 1, bootstrap: 0 }} />);
    const legend = within(
      screen.getByRole("region", {
        name: "Most changes here apply on save; some wait for a restart",
      }),
    );
    expect(legend.getByText("9 settings")).toBeInTheDocument();
    expect(legend.getByText("1 setting")).toBeInTheDocument();
    expect(legend.getByText(/nothing restarts and no one is disconnected/)).toBeInTheDocument();
    expect(legend.getByText(/restart the replicas one at a time/)).toBeInTheDocument();
    expect(legend.queryByText("Per replica (file or environment)")).not.toBeInTheDocument();
  });

  it("shows nothing for a section with no settings", () => {
    const { container } = render(<AppliesLegend counts={{ hot: 0, restart: 0, bootstrap: 0 }} />);
    expect(container).toBeEmptyDOMElement();
  });
});

describe("RateLimitsNote", () => {
  it("says what a client sees and that each replica counts on its own", () => {
    render(<RateLimitsNote />);
    const note = within(screen.getByRole("region", { name: "How rate limits work" }));
    expect(note.getByText("M_LIMIT_EXCEEDED")).toBeInTheDocument();
    expect(note.getByText(/refused with HTTP 429/)).toBeInTheDocument();
    expect(note.getByText("In a cluster, each replica counts on its own.")).toBeInTheDocument();
  });
});
