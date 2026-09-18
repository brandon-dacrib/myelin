import { useMemo, useState } from "react";
import { useNavigate, useSearch, Link } from "@tanstack/react-router";
import { Cable, Play, Pause } from "lucide-react";
import {
  useAppservices,
  usePauseAppservice,
  useResumeAppservice,
  deriveDisplayName,
  deriveKindLabel,
  type AppService,
} from "@/api/bridges";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { Select } from "@/components/ui/select/Select";
import { DataTable, type Column, type SortState } from "@/components/ui/table/DataTable";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ErrorState, ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import { bridgeHealthMeta, healthKeyOf } from "@/lib/bridge-state";

const HEALTH_OPTIONS = [
  { value: "healthy", label: "Healthy" },
  { value: "degraded", label: "Degraded" },
  { value: "down", label: "Down" },
  { value: "paused", label: "Paused" },
  { value: "unknown", label: "Unknown" },
];

/**
 * `/bridges` — "are the bridges connected and keeping up?"
 * (information-architecture.md, Bridges). `GET /appservices` has no
 * `state`/`kind` filter parameter (only free-text `q`), so the health
 * filter below applies client-side to the loaded page only; see the
 * reconciliation note in api/bridges.ts and
 * docs/status/16-management-web-interface.md.
 */
export function BridgesListPage() {
  const search = useSearch({ from: "/bridges" });
  const navigate = useNavigate({ from: "/bridges" });
  const [sort, setSort] = useState<SortState | undefined>();
  const [cursorStack, setCursorStack] = useState<(string | undefined)[]>([]);
  const canRead = hasScope("bridges:read");

  // Every hook below runs unconditionally regardless of scope (rules of
  // hooks); the scope gate only affects what is rendered, further down.
  const { data, isLoading, isError, refetch } = useAppservices({
    cursor: search.cursor,
    limit: 20,
  });

  const pause = usePauseAppservice();
  const resume = useResumeAppservice();

  const rows = useMemo(() => {
    let items = data?.items ?? [];
    if (search.state) {
      items = items.filter((b) =>
        search.state === "paused" ? b.paused : !b.paused && b.health === search.state,
      );
    }
    if (!sort) return items;
    const sorted = [...items].sort((a, b) => {
      if (sort.key === "name") return deriveDisplayName(a).localeCompare(deriveDisplayName(b));
      return 0;
    });
    return sort.direction === "desc" ? sorted.reverse() : sorted;
  }, [data, sort, search.state]);

  const columns: Column<AppService>[] = [
    {
      key: "name",
      header: "Name",
      sortable: true,
      priority: 1,
      // Renders a real link: this is the row's desktop activation control
      // (DataTable's onRowClick doc comment explains why the row itself
      // isn't one).
      interactive: true,
      render: (b) => (
        <Link
          to="/bridges/$bridgeId"
          params={{ bridgeId: b.id ?? "" }}
          className="font-medium text-text hover:text-accent hover:underline"
        >
          {deriveDisplayName(b)}
        </Link>
      ),
    },
    { key: "kind", header: "Kind", priority: 2, render: (b) => deriveKindLabel(b) },
    {
      key: "state",
      header: "State",
      priority: 1,
      render: (b) => {
        const meta = bridgeHealthMeta[healthKeyOf(b)];
        return <Badge status={meta.status}>{meta.label}</Badge>;
      },
      renderCompact: (b) => bridgeHealthMeta[healthKeyOf(b)].label,
    },
    {
      key: "sender_localpart",
      header: "Sender",
      priority: 2,
      render: (b) => <span className="font-identifier">{b.sender_localpart}</span>,
    },
    {
      key: "created_at",
      header: "Created",
      priority: 3,
      render: (b) => (b.created_at ? new Date(b.created_at).toLocaleDateString() : "\u2014"),
    },
    {
      key: "actions",
      header: "Actions",
      priority: 1,
      // Renders real buttons: must stay outside the card fallback's tap
      // target (see the `interactive` doc comment on Column).
      interactive: true,
      render: (b) => (
        <div className="flex justify-end gap-1">
          {b.paused ? (
            <Button
              variant="ghost"
              size="icon"
              aria-label={`Resume ${deriveDisplayName(b)}`}
              disabled={!hasScope("bridges:write")}
              title={!hasScope("bridges:write") ? "Needs bridges:write" : undefined}
              onClick={() => {
                resume.mutate(b.id ?? "", {
                  onSuccess: () => toast({ title: `Bridge ${deriveDisplayName(b)} resumed` }),
                  onError: () =>
                    toast({ title: `Couldn't resume ${deriveDisplayName(b)}`, variant: "danger" }),
                });
              }}
            >
              <Play size={14} aria-hidden="true" />
            </Button>
          ) : (
            <Button
              variant="ghost"
              size="icon"
              aria-label={`Pause ${deriveDisplayName(b)}`}
              disabled={!hasScope("bridges:write")}
              title={!hasScope("bridges:write") ? "Needs bridges:write" : undefined}
              onClick={() => {
                pause.mutate(b.id ?? "", {
                  onSuccess: () => toast({ title: `Bridge ${deriveDisplayName(b)} paused` }),
                  onError: () =>
                    toast({ title: `Couldn't pause ${deriveDisplayName(b)}`, variant: "danger" }),
                });
              }}
            >
              <Pause size={14} aria-hidden="true" />
            </Button>
          )}
        </div>
      ),
    },
  ];

  if (!canRead) {
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Bridges</h1>
        <ForbiddenState scope="bridges:read" />
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h1 className="text-xl text-text">Bridges</h1>
        <Button
          disabled={!hasScope("bridges:write")}
          title={!hasScope("bridges:write") ? "Needs bridges:write" : undefined}
          onClick={() => navigate({ to: "/bridges/new" })}
        >
          Add bridge
        </Button>
      </div>

      <div className="mt-4 flex flex-wrap gap-3">
        <div className="w-48">
          <Select
            aria-label="Filter by state"
            placeholder="All states"
            options={HEALTH_OPTIONS}
            value={search.state ?? ""}
            onValueChange={(v) => navigate({ search: { ...search, state: v || undefined } })}
          />
        </div>
        {search.state && (
          <Button variant="ghost" onClick={() => navigate({ search: {} })}>
            Clear filters
          </Button>
        )}
      </div>

      {isError && (
        <div className="mt-6">
          <ErrorState title="Couldn't load bridges" onRetry={() => refetch()} />
        </div>
      )}

      {!isError && (
        <div className="mt-4">
          <DataTable
            caption="Bridges"
            columns={columns}
            rows={rows}
            getRowId={(b) => b.id ?? ""}
            loading={isLoading}
            sort={sort}
            onSortChange={setSort}
            onRowClick={(b) =>
              navigate({ to: "/bridges/$bridgeId", params: { bridgeId: b.id ?? "" } })
            }
            empty={
              search.state ? (
                <EmptyState
                  variant="filtered"
                  icon={<Cable aria-hidden="true" />}
                  title="No bridges match these filters"
                  action={
                    <Button variant="ghost" onClick={() => navigate({ search: {} })}>
                      Clear filters
                    </Button>
                  }
                />
              ) : (
                <EmptyState
                  icon={<Cable aria-hidden="true" />}
                  title="No bridges yet"
                  description="Bridges connect WhatsApp, Signal, Telegram and other networks to this server."
                  action={
                    hasScope("bridges:write") ? (
                      <Button onClick={() => navigate({ to: "/bridges/new" })}>Add bridge</Button>
                    ) : undefined
                  }
                />
              )
            }
            pagination={{
              hasPrevious: cursorStack.length > 0,
              hasNext: Boolean(data?.next_cursor),
              onPrevious: () => {
                const prev = cursorStack.at(-1);
                setCursorStack((s) => s.slice(0, -1));
                navigate({ search: { ...search, cursor: prev } });
              },
              onNext: () => {
                if (!data?.next_cursor) return;
                setCursorStack((s) => [...s, search.cursor]);
                navigate({ search: { ...search, cursor: data.next_cursor ?? undefined } });
              },
            }}
          />
        </div>
      )}
    </div>
  );
}
