import { describe, expect, it, vi } from "vitest";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { DataTable, type Column } from "./DataTable";

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

  it("calls onRowClick when a row is activated", async () => {
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
    expect(onRowClick).toHaveBeenCalledWith(rows[0]);
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
