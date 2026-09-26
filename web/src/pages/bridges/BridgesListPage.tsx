import { useMemo, useState } from "react";
import { useNavigate, useSearch, Link } from "@tanstack/react-router";
import { Cable, Play, Pause } from "lucide-react";
import {
  useAppservices,
  useBridgeTypes,
  usePauseAppservice,
  useResumeAppservice,
  type AppService,
  type AppServiceHealthStatus,
} from "@/api/bridges";
import { useServerInfo } from "@/api/dashboard";
import { BridgeGlyph } from "@/components/BridgeGlyph";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { DataTable, type Column, type SortState } from "@/components/ui/table/DataTable";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { QueryProblemState } from "@/components/QueryProblemState";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import {
  bridgeKind,
  bridgeTitle,
  bridgeTypeOf,
  botMatrixId,
  healthCounts,
  sortByAttention,
} from "@/lib/bridge-catalogue";
import { bridgeHealthMeta, healthKeyOf } from "@/lib/bridge-state";
import { cn } from "@/lib/cn";

const HEALTH_ORDER: AppServiceHealthStatus[] = ["down", "degraded", "unknown", "healthy", "paused"];

/**
 * `/bridges` — "are the bridges connected and keeping up?"
 * (information-architecture.md, Bridges). The summary strip answers that before the table
 * does, and the table reads attention-first. `GET /appservices` has no `state`/`kind` filter
 * parameter (only free-text `q`), so the health filter applies client-side to the loaded page
 * only; see the reconciliation note in api/bridges.ts and
 * docs/status/16-management-web-interface.md.
 */
export function BridgesListPage() {
  const search = useSearch({ from: "/bridges" });
  const navigate = useNavigate({ from: "/bridges" });
  const [sort, setSort] = useState<SortState | undefined>();
  const [cursorStack, setCursorStack] = useState<(string | undefined)[]>([]);
  const canRead = hasScope("bridges:read");
  const canWrite = hasScope("bridges:write");

  // Every hook below runs unconditionally regardless of scope (rules of
  // hooks); the scope gate only affects what is rendered, further down.
  const { data, isLoading, isError, error, refetch } = useAppservices({
    cursor: search.cursor,
    limit: 20,
  });
  const { data: types } = useBridgeTypes();
  const { data: server } = useServerInfo();

  const pause = usePauseAppservice();
  const resume = useResumeAppservice();

  const all = useMemo(() => data?.items ?? [], [data]);
  const counts = useMemo(() => healthCounts(all), [all]);

  const rows = useMemo(() => {
    let items = all;
    if (search.state) {
      items = items.filter((b) =>
        search.state === "paused" ? b.paused : !b.paused && b.health === search.state,
      );
    }
    if (!sort) return sortByAttention(items);
    const sorted = [...items].sort((a, b) => {
      if (sort.key === "name")
        return bridgeTitle(a, bridgeTypeOf(a, types)).localeCompare(
          bridgeTitle(b, bridgeTypeOf(b, types)),
        );
      return 0;
    });
    return sort.direction === "desc" ? sorted.reverse() : sorted;
  }, [all, sort, search.state, types]);

  const columns: Column<AppService>[] = [
    {
      key: "name",
      header: "Bridge",
      sortable: true,
      priority: 1,
      // Renders a real link: this is the row's desktop activation control
      // (DataTable's onRowClick doc comment explains why the row itself
      // isn't one).
      interactive: true,
      render: (b) => {
        const type = bridgeTypeOf(b, types);
        return (
          <span className="flex items-center gap-3">
            <BridgeGlyph category={type?.category} size="sm" />
            <span className="flex min-w-0 flex-col">
              <Link
                to="/bridges/$bridgeId"
                params={{ bridgeId: b.id ?? "" }}
                className="truncate font-medium text-text hover:text-accent hover:underline"
              >
                {bridgeTitle(b, type)}
              </Link>
              <span className="truncate text-xs text-text-muted">{bridgeKind(b, type)}</span>
            </span>
          </span>
        );
      },
      renderCompact: (b) => bridgeTitle(b, bridgeTypeOf(b, types)),
    },
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
      key: "bot",
      header: "Bot",
      priority: 2,
      render: (b) => (
        <span className="font-identifier text-text-muted">
          {botMatrixId(b.sender_localpart, server?.name)}
        </span>
      ),
    },
    {
      key: "created_at",
      header: "Added",
      priority: 3,
      render: (b) => (b.created_at ? new Date(b.created_at).toLocaleDateString() : "—"),
    },
    {
      key: "actions",
      header: "Actions",
      priority: 1,
      align: "end",
      // Renders real buttons: must stay outside the card fallback's tap
      // target (see the `interactive` doc comment on Column).
      interactive: true,
      render: (b) => {
        const title = bridgeTitle(b, bridgeTypeOf(b, types));
        return (
          <div className="flex justify-end gap-1">
            {b.paused ? (
              <Button
                variant="ghost"
                size="icon"
                aria-label={`Resume ${title}`}
                disabled={!canWrite}
                title={!canWrite ? "Needs bridges:write" : undefined}
                onClick={() => {
                  resume.mutate(b.id ?? "", {
                    onSuccess: () => toast({ title: `Bridge ${title} resumed` }),
                    onError: () => toast({ title: `Couldn't resume ${title}`, variant: "danger" }),
                  });
                }}
              >
                <Play size={14} aria-hidden="true" />
              </Button>
            ) : (
              <Button
                variant="ghost"
                size="icon"
                aria-label={`Pause ${title}`}
                disabled={!canWrite}
                title={!canWrite ? "Needs bridges:write" : undefined}
                onClick={() => {
                  pause.mutate(b.id ?? "", {
                    onSuccess: () => toast({ title: `Bridge ${title} paused` }),
                    onError: () => toast({ title: `Couldn't pause ${title}`, variant: "danger" }),
                  });
                }}
              >
                <Pause size={14} aria-hidden="true" />
              </Button>
            )}
          </div>
        );
      },
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
        <div>
          <h1 className="text-xl text-text">Bridges</h1>
          <p className="mt-0.5 text-sm text-text-muted">
            Other networks, connected to this server. Each bridge runs as its own process and
            registers here.
          </p>
        </div>
        <Button
          disabled={!canWrite}
          title={!canWrite ? "Needs bridges:write" : undefined}
          onClick={() => navigate({ to: "/bridges/new" })}
        >
          Add bridge
        </Button>
      </div>

      {!isError && all.length > 0 && (
        <div className="mt-5 flex flex-wrap items-center gap-2" aria-label="Filter by state">
          <FilterChip
            label="All"
            count={all.length}
            active={!search.state}
            onClick={() => navigate({ search: { ...search, state: undefined } })}
          />
          {HEALTH_ORDER.filter((h) => counts[h] > 0).map((h) => (
            <FilterChip
              key={h}
              label={bridgeHealthMeta[h].label}
              count={counts[h]}
              status={bridgeHealthMeta[h].status}
              active={search.state === h}
              onClick={() =>
                navigate({ search: { ...search, state: search.state === h ? undefined : h } })
              }
            />
          ))}
        </div>
      )}

      {isError && (
        <div className="mt-6">
          <QueryProblemState
            error={error}
            resource="bridges"
            scope="bridges:read"
            onRetry={() => refetch()}
          />
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
                  title="No bridges in this state"
                  action={
                    <Button
                      variant="ghost"
                      onClick={() => navigate({ search: { ...search, state: undefined } })}
                    >
                      Show all
                    </Button>
                  }
                />
              ) : (
                <EmptyState
                  icon={<Cable aria-hidden="true" />}
                  title="No bridges yet"
                  description="Connect WhatsApp, Signal, Telegram, Discord, Slack, IRC and more. Adding one takes a minute; each person then signs in from a chat with the bridge's bot."
                  docsHref="https://docs.mau.fi/bridges/"
                  action={
                    canWrite ? (
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

/** One state and how many bridges are in it; pressing it filters the table to that state. */
function FilterChip({
  label,
  count,
  status,
  active,
  onClick,
}: {
  label: string;
  count: number;
  status?: "success" | "warning" | "danger" | "info" | "muted" | "neutral";
  active: boolean;
  onClick: () => void;
}) {
  const dot = {
    success: "bg-success",
    warning: "bg-warning",
    danger: "bg-danger",
    info: "bg-info",
    muted: "bg-muted-status",
    neutral: "bg-text-faint",
  }[status ?? "neutral"];
  return (
    <button
      type="button"
      aria-pressed={active}
      onClick={onClick}
      className={cn(
        "inline-flex items-center gap-2 rounded-full border px-3 py-1 text-sm transition-colors duration-fast",
        active
          ? "border-accent bg-accent-muted text-accent"
          : "border-border bg-surface text-text-muted hover:bg-surface-sunken hover:text-text",
        "focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
      )}
    >
      {status && <span aria-hidden="true" className={cn("size-2 rounded-full", dot)} />}
      {label}
      <span className="tabular-nums">{count}</span>
    </button>
  );
}
