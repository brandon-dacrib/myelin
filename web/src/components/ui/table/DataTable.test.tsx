import { describe, expect, it, vi, beforeEach, afterEach } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { DataTable, type Column } from "./DataTable";
import { Button } from "../button/Button";

interface Row {
  id: string;
  name: string;
}

const columns: Column<Row>[] = [
  { key: "name", header: "Name", sortable: true, render: (r) => r.name },
];
const rows: Row[] = [
  { id: "b", name: "Bravo" },
  { id: "a", name: "Alpha" },
];

describe("DataTable", () => {
  it("renders a row per item with the caption for screen readers", () => {
    render(<DataTable columns={columns} rows={rows} getRowId={(r) => r.id} caption="Bridges" />);
    const table = within(screen.getByRole("table"));
    expect(table.getByText("Bravo")).toBeInTheDocument();
    expect(table.getByText("Alpha")).toBeInTheDocument();
    expect(table.getByText("Bridges")).toBeInTheDocument();
  });

  it("calls onSortChange with the next direction when a sortable header is clicked", async () => {
    const onSortChange = vi.fn();
    render(
      <DataTable
        columns={columns}
        rows={rows}
        getRowId={(r) => r.id}
        caption="Bridges"
        onSortChange={onSortChange}
      />,
    );
    const table = within(screen.getByRole("table"));
    await userEvent.click(table.getByRole("button", { name: /Name/ }));
    expect(onSortChange).toHaveBeenCalledWith({ key: "name", direction: "asc" });
  });

  it("calls onRowClick when the <768px card fallback is activated", async () => {
    // The desktop table's <tr> is deliberately not clickable (see the
    // onRowClick doc comment on DataTable): only the card fallback (and any
    // real link/button an `interactive` column renders) activates a row.
    const onRowClick = vi.fn();
    render(
      <DataTable
        columns={columns}
        rows={rows}
        getRowId={(r) => r.id}
        caption="Bridges"
        onRowClick={onRowClick}
      />,
    );
    const cardList = within(screen.getByRole("list"));
    await userEvent.click(cardList.getByText("Bravo"));
    expect(onRowClick).toHaveBeenCalledWith(rows[0]);
  });

  it("does not attach a click handler to the desktop row itself", async () => {
    const onRowClick = vi.fn();
    render(
      <DataTable
        columns={columns}
        rows={rows}
        getRowId={(r) => r.id}
        caption="Bridges"
        onRowClick={onRowClick}
      />,
    );
    const table = within(screen.getByRole("table"));
    await userEvent.click(table.getByText("Bravo"));
    expect(onRowClick).not.toHaveBeenCalled();
  });

  describe("interactive columns and the card fallback", () => {
    let errorSpy: ReturnType<typeof vi.spyOn>;

    beforeEach(() => {
      // React logs invalid DOM nesting (e.g. a <button> inside a <button>)
      // via console.error. Spying lets the test fail loudly if it happens,
      // rather than the warning scrolling past silently — this is the
      // regression coverage for the nested-button defect on the bridges
      // list page (DataTable's card fallback wrapping an interactive
      // "actions" column inside its own tap-target button).
      errorSpy = vi.spyOn(console, "error").mockImplementation(() => {});
    });

    afterEach(() => {
      errorSpy.mockRestore();
    });

    it("keeps an interactive column's controls outside the card's tap-target button", () => {
      const withActions: Column<Row>[] = [
        ...columns,
        {
          key: "actions",
          header: "Actions",
          interactive: true,
          render: (r) => <Button aria-label={`Pause ${r.name}`}>Pause</Button>,
        },
      ];
      render(
        <DataTable
          columns={withActions}
          rows={rows}
          getRowId={(r) => r.id}
          caption="Bridges"
          onRowClick={() => {}}
        />,
      );

      const cardList = screen.getByRole("list");
      const pauseButton = within(cardList).getByRole("button", { name: "Pause Bravo" });
      // The action button must not be a descendant of the card's own
      // tap-target <button> (that nesting is what produced the defect).
      const outerButton = pauseButton.closest("li")?.querySelector(":scope > button");
      expect(outerButton).not.toBeNull();
      expect(outerButton?.contains(pauseButton)).toBe(false);

      const nestingWarning = errorSpy.mock.calls.some((call: unknown[]) =>
        String(call[0]).match(/cannot (be a descendant of|contain a nested)/i),
      );
      expect(nestingWarning).toBe(false);
    });
  });

  it("renders the empty state instead of the table when there are no rows", () => {
    render(
      <DataTable
        columns={columns}
        rows={[]}
        getRowId={(r) => r.id}
        caption="Bridges"
        empty={<p>No bridges yet</p>}
      />,
    );
    expect(screen.getByText("No bridges yet")).toBeInTheDocument();
    expect(screen.queryByText("Bravo")).not.toBeInTheDocument();
  });

  it("disables Previous/Next according to the pagination state", () => {
    render(
      <DataTable
        columns={columns}
        rows={rows}
        getRowId={(r) => r.id}
        caption="Bridges"
        pagination={{ hasPrevious: false, hasNext: true, onPrevious: () => {}, onNext: () => {} }}
      />,
    );
    expect(screen.getByRole("button", { name: "Previous" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Next" })).toBeEnabled();
  });
});
