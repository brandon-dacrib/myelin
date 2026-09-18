import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { CopyableId } from "./CopyableId";

describe("CopyableId", () => {
  const writeText = vi.fn().mockResolvedValue(undefined);

  beforeEach(() => {
    Object.assign(navigator, { clipboard: { writeText } });
  });

  afterEach(() => {
    writeText.mockClear();
  });

  it("shows the identifier in monospace and copies it on click", async () => {
    render(<CopyableId value="@alice:example.org" />);
    expect(screen.getByText("@alice:example.org")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("button", { name: /Copy @alice:example.org/ }));
    expect(writeText).toHaveBeenCalledWith("@alice:example.org");
    await waitFor(() => expect(screen.getByRole("button", { name: "Copied" })).toBeInTheDocument());
  });
});
