import { useState } from "react";
import type { Meta, StoryObj } from "@storybook/react-vite";
import { Cable, Pause, Play } from "lucide-react";
import { DataTable, type Column, type SortState } from "./DataTable";
import { Badge } from "../badge/Badge";
import { Button } from "../button/Button";
import { EmptyState } from "../empty-state/EmptyState";

interface Row {
  id: string;
  name: string;
  kind: string;
  state: "running" | "paused" | "bridge_unreachable";
  backlog: number;
  lastSuccess: string;
}

const rows: Row[] = [
  {
    id: "whatsapp",
    name: "WhatsApp",
    kind: "mautrix-whatsapp",
    state: "running",
    backlog: 0,
    lastSuccess: "1 min ago",
  },
  {
    id: "telegram",
    name: "Telegram",
    kind: "mautrix-telegram",
    state: "running",
    backlog: 214,
    lastSuccess: "6 min ago",
  },
  {
    id: "signal",
    name: "Signal",
    kind: "mautrix-signal",
    state: "bridge_unreachable",
    backlog: 1402,
    lastSuccess: "3 h ago",
  },
  {
    id: "discord",
    name: "Discord",
    kind: "mautrix-discord",
    state: "paused",
    backlog: 0,
    lastSuccess: "20 h ago",
  },
];

const statusMap = {
  running: "success",
  paused: "muted",
  bridge_unreachable: "danger",
} as const;

const columns: Column<Row>[] = [
  { key: "name", header: "Name", sortable: true, priority: 1, render: (r) => r.name },
  { key: "kind", header: "Kind", priority: 2, render: (r) => r.kind },
  {
    key: "state",
    header: "State",
    priority: 1,
    render: (r) => <Badge status={statusMap[r.state]}>{r.state.replace(/_/g, " ")}</Badge>,
  },
  {
    key: "backlog",
    header: "Backlog",
    sortable: true,
    align: "end",
    priority: 2,
    render: (r) => r.backlog,
  },
  { key: "lastSuccess", header: "Last success", priority: 3, render: (r) => r.lastSuccess },
];

const meta: Meta = { title: "Primitives/DataTable" };
export default meta;

export const Default: StoryObj = {
  render: () => {
    function Demo() {
      const [sort, setSort] = useState<SortState | undefined>(undefined);
      return (
        <DataTable
          columns={columns}
          rows={rows}
          getRowId={(r) => r.id}
          caption="Bridges"
          sort={sort}
          onSortChange={setSort}
          onRowClick={() => {}}
        />
      );
    }
    return <Demo />;
  },
};

export const Loading: StoryObj = {
  render: () => (
    <DataTable columns={columns} rows={[]} getRowId={(r) => r.id} caption="Bridges" loading />
  ),
};

export const Empty: StoryObj = {
  render: () => (
    <DataTable
      columns={columns}
      rows={[]}
      getRowId={(r) => r.id}
      caption="Bridges"
      empty={
        <EmptyState
          icon={<Cable aria-hidden="true" />}
          title="No bridges yet"
          description="Bridges connect WhatsApp, Signal, Telegram and other networks to this server."
        />
      }
    />
  ),
};

export const WithPagination: StoryObj = {
  render: () => (
    <DataTable
      columns={columns}
      rows={rows}
      getRowId={(r) => r.id}
      caption="Bridges"
      pagination={{
        hasPrevious: false,
        hasNext: true,
        onPrevious: () => {},
        onNext: () => {},
        pageLabel: "Page 1",
      }}
    />
  ),
};

export const Compact: StoryObj = {
  render: () => (
    <DataTable
      columns={columns}
      rows={rows}
      getRowId={(r) => r.id}
      caption="Bridges"
      density="compact"
    />
  ),
};

/**
 * A row with its own real controls (a link-equivalent name and action
 * buttons), marked `interactive: true` so the <768px card fallback keeps
 * them outside its tap-target button instead of nesting interactive
 * elements inside one another — invalid HTML that is unreliable for
 * keyboard and assistive-technology users. Resize the preview below 768px
 * to see the card layout keep the actions row separate from the tap
 * target. (Automated regression coverage for this — a console guard that
 * fails on React's "invalid DOM nesting" warning, plus an axe pass at a
 * phone viewport — lives in `e2e/add-bridge.spec.ts`, since Storybook's
 * own preview iframe does not resize.)
 */
export const WithInteractiveColumns: StoryObj = {
  render: () => {
    const interactiveColumns: Column<Row>[] = [
      ...columns.slice(0, -1),
      {
        key: "actions",
        header: "Actions",
        priority: 1,
        interactive: true,
        render: (r) => (
          <div className="flex justify-end gap-1">
            <Button variant="ghost" size="icon" aria-label={`Pause ${r.name}`}>
              <Pause size={14} aria-hidden="true" />
            </Button>
            <Button variant="ghost" size="icon" aria-label={`Resume ${r.name}`}>
              <Play size={14} aria-hidden="true" />
            </Button>
          </div>
        ),
      },
    ];
    return (
      <DataTable
        columns={interactiveColumns}
        rows={rows}
        getRowId={(r) => r.id}
        caption="Bridges"
        onRowClick={() => {}}
      />
    );
  },
};
