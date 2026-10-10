import { useState } from "react";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { Globe } from "lucide-react";
import { useFederationDestinations } from "@/api/dashboard";
import type { Destination } from "@/api/federation";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { toast } from "@/components/ui/toast/toast-store";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { hasScope } from "@/lib/auth";
import { cn } from "@/lib/cn";
import { DESTINATION_PAGE_SIZE, destinationHealth, forgottenSummary } from "@/lib/federation";
import { formatCount } from "@/lib/format";
import { fromSortState, toSortState } from "@/lib/sort-param";
import { useFederationQueueLimit } from "@/api/federation";
import { CatchUpBadge } from "./federation/CatchUp";
import {
  failingParam,
  sharesRoomParam,
  type DestinationShow,
  type FederationSearch,
} from "./federation/federation-search";
import { OwnKeysPanel } from "./federation/FederationPanels";
import { ForgetDestinationDialog } from "./federation/ForgetDestination";
import { PrunePanel } from "./federation/PruneDestinations";

const SHOW_OPTIONS: { value: DestinationShow | "all"; label: string }[] = [
  { value: "all", label: "Every server" },
  { value: "failing", label: "Failing" },
  { value: "not-failing", label: "Not failing" },
  { value: "no-shared-room", label: "No shared room" },
];

/**
 * `/federation` — flows.md flow 4: watch federation health. The server does the filtering
 * (`failing`), the ordering (`sort`) and the paging (`cursor`), so a server with thousands of
 * destinations is read a page at a time and the counts are the whole list's, never a page's.
 */
export function FederationPage() {
  const search = useSearch({ from: "/federation" });
  const navigate = useNavigate({ from: "/federation" });
  const canRead = hasScope("admin:read");
  const canWrite = hasScope("admin:write");
  const [forgetting, setForgetting] = useState<Destination | null>(null);
  const update = (patch: Partial<FederationSearch>) =>
    navigate({ search: { ...search, ...patch } });

  // Without a sort the server lists failing servers first, then the rest by name; among only
  // the failing ones, longest failing first is the order an operator wants.
  const sort = search.sort ?? (search.show === "failing" ? "failing_since" : undefined);
  const { data, isLoading, isError, error, refetch, isPlaceholderData } = useFederationDestinations(
    {
      limit: DESTINATION_PAGE_SIZE,
      cursor: search.cursor,
      sort,
      failing: failingParam(search.show),
      shares_room: sharesRoomParam(search.show),
      include_total: true,
    },
  );

  const rows = data?.items ?? [];
  const paged = Boolean(search.cursor || data?.next_cursor);
  const catchingUp = rows.filter((d) => d.catch_up_since).length;

  const columns: Column<Destination>[] = [
    {
      key: "server_name",
      header: "Server",
      priority: 1,
      interactive: true,
      sortable: true,
      render: (d) => (
        <Link
          to="/federation/$serverName"
          params={{ serverName: d.server_name ?? "" }}
          className="font-identifier font-medium text-text hover:text-accent hover:underline"
        >
          {d.server_name}
        </Link>
      ),
    },
    {
      // Sorted by when the failures began: longest failing first, then the servers that are
      // not failing (which have no such time and sort last either way).
      key: "failing_since",
      header: "Status",
      priority: 1,
      sortable: true,
      render: (d) => {
        const meta = destinationHealth(d);
        return (
          <div className="flex flex-wrap items-center gap-1.5">
            <Badge status={meta.status}>{meta.label}</Badge>
            <CatchUpBadge since={d.catch_up_since} />
          </div>
        );
      },
      renderCompact: (d) =>
        d.catch_up_since
          ? `${destinationHealth(d).label}, catching up`
          : destinationHealth(d).label,
    },
    {
      key: "last_successful_at",
      header: "Last success",
      priority: 2,
      sortable: true,
      render: (d) => <RelativeTime at={d.last_successful_at} />,
    },
    {
      // Sorted by queued events (PDUs); the other messages (EDUs) ride along.
      key: "pending_pdu_count",
      header: "Waiting to send",
      priority: 3,
      align: "end",
      sortable: true,
      render: (d) =>
        d.catch_up_since ? (
          <span className="text-text-muted">not queued</span>
        ) : (
          (d.pending_pdu_count ?? 0) + (d.pending_edu_count ?? 0)
        ),
    },
    {
      // Not sortable: the server does not sort by it (it is read from the rooms, not stored).
      key: "shared_rooms_count",
      header: "Shared rooms",
      priority: 2,
      align: "end",
      render: (d) =>
        d.shared_rooms_count == null ? (
          <span className="text-text-muted" title="This server cannot say right now">
            unknown
          </span>
        ) : d.shared_rooms_count === 0 ? (
          <span className="text-text-muted">none</span>
        ) : (
          d.shared_rooms_count
        ),
    },
    {
      key: "forget",
      header: "Action",
      priority: 2,
      align: "end",
      interactive: true,
      render: (d) => (
        <Button
          variant="ghost"
          size="sm"
          disabled={!canWrite}
          title={!canWrite ? "Needs admin:write" : `Forget ${d.server_name}`}
          aria-label={`Forget ${d.server_name}`}
          onClick={() => setForgetting(d)}
        >
          Forget
        </Button>
      ),
    },
  ];

  if (!canRead) {
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Federation</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );
  }

  const show = search.show ?? "all";
  const total = data?.total;
  const countLabel =
    total == null
      ? undefined
      : `${formatCount(total)} ${total === 1 ? "server" : "servers"}${
          show === "failing"
            ? " failing"
            : show === "not-failing"
              ? " not failing"
              : show === "no-shared-room"
                ? " sharing no room"
                : ""
        }`;

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <h1 className="text-xl text-text">Federation</h1>
      <p className="mt-1 max-w-3xl text-sm text-text-muted">
        The other Matrix servers this one sends to: every server with a user in a room your users
        are in. Each row says whether sending to it works now; open one for its shared rooms, its
        signing keys and its retry state.
      </p>

      <StatusKey catchingUp={catchingUp} paged={paged} />

      {isError && (
        <div className="mt-6">
          <QueryProblemState
            error={error}
            resource="federation destinations"
            scope="admin:read"
            onRetry={() => refetch()}
          />
        </div>
      )}

      {!isError && (
        <div className="mt-4">
          <div className="mb-3 flex flex-wrap items-center justify-between gap-3">
            <div
              role="group"
              aria-label="Show"
              className="inline-flex h-9 overflow-hidden rounded-sm border border-border-strong"
            >
              {SHOW_OPTIONS.map((option) => (
                <button
                  key={option.value}
                  type="button"
                  aria-pressed={show === option.value}
                  onClick={() =>
                    update({
                      show: option.value === "all" ? undefined : option.value,
                      cursor: undefined,
                    })
                  }
                  className={cn(
                    "px-3 text-sm font-medium focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--color-focus)]",
                    show === option.value
                      ? "bg-accent-muted text-accent"
                      : "bg-surface text-text-muted hover:bg-surface-sunken hover:text-text",
                  )}
                >
                  {option.label}
                </button>
              ))}
            </div>
            <p className="text-sm text-text-muted" aria-live="polite">
              {search.sort
                ? "Sorted by the column you chose; click it again to turn the order round."
                : sort
                  ? "Longest failing first."
                  : "Failing servers first, then the rest by name."}
            </p>
          </div>
          <DataTable
            caption="Federation destinations"
            columns={columns}
            rows={rows}
            getRowId={(d) => d.server_name ?? ""}
            loading={isLoading}
            className={cn(isPlaceholderData && "opacity-60 transition-opacity")}
            sort={sort ? toSortState(sort) : undefined}
            onSortChange={(next) => update({ sort: fromSortState(next), cursor: undefined })}
            onRowClick={(d) =>
              navigate({
                to: "/federation/$serverName",
                params: { serverName: d.server_name ?? "" },
              })
            }
            pagination={{
              hasPrevious: Boolean(search.cursor),
              hasNext: Boolean(data?.next_cursor),
              onPrevious: () => update({ cursor: data?.prev_cursor ?? undefined }),
              onNext: () => update({ cursor: data?.next_cursor ?? undefined }),
              pageLabel: countLabel,
            }}
            empty={<NoDestinations show={show} />}
          />
        </div>
      )}

      <PrunePanel />
      <OwnKeysPanel />

      {forgetting && (
        <ForgetDestinationDialog
          destination={forgetting}
          open
          onOpenChange={(open) => {
            if (!open) setForgetting(null);
          }}
          onForgotten={(result) =>
            toast({
              title: `Forgot ${result.server_name}`,
              description: forgottenSummary(result),
            })
          }
        />
      )}
    </div>
  );
}

function NoDestinations({ show }: { show: DestinationShow | "all" }) {
  if (show === "failing")
    return (
      <EmptyState
        variant="filtered"
        icon={<Globe aria-hidden="true" />}
        title="No failing servers"
        description="Every server this one sends to answered its last request."
      />
    );
  if (show === "not-failing")
    return (
      <EmptyState
        variant="filtered"
        icon={<Globe aria-hidden="true" />}
        title="Every known server is failing"
        description="No server this one sends to has answered its last request."
      />
    );
  if (show === "no-shared-room")
    return (
      <EmptyState
        variant="filtered"
        icon={<Globe aria-hidden="true" />}
        title="Every known server shares a room"
        description="There is nothing to forget: each server this one knows has users in a room your users are in."
      />
    );
  return (
    <EmptyState
      icon={<Globe aria-hidden="true" />}
      title="No federation traffic yet"
      description="When your users join rooms on other servers, those servers appear here."
    />
  );
}

/**
 * What each status means, once, above the table: an operator should not need the docs to read
 * a badge. Catch-up is explained in full when a destination is in it.
 */
function StatusKey({ catchingUp, paged }: { catchingUp: number; paged: boolean }) {
  const { limit } = useFederationQueueLimit();
  const where = paged ? " on this page" : "";
  return (
    <details className="mt-3 max-w-3xl text-sm text-text-muted" open={catchingUp > 0}>
      <summary className="cursor-pointer text-accent hover:underline">
        What the statuses mean
        {catchingUp > 0 &&
          ` (${catchingUp} ${catchingUp === 1 ? "server is" : "servers are"} catching up${where})`}
      </summary>
      <dl className="mt-2 grid gap-2 sm:grid-cols-[10rem_1fr]">
        {(["success", "warning", "danger"] as const).map((status) => {
          const sample = destinationHealth(
            status === "danger"
              ? { failing_since: "x" }
              : status === "warning"
                ? { retry_interval_ms: 1 }
                : {},
          );
          return (
            <div key={status} className="contents">
              <dt>
                <Badge status={sample.status}>{sample.label}</Badge>
              </dt>
              <dd>{sample.explanation}</dd>
            </div>
          );
        })}
        <div className="contents">
          <dt>
            <Badge status="info" hideIcon>
              Catching up
            </Badge>
          </dt>
          <dd>
            The server was unreachable for longer than its queue holds (
            {limit.toLocaleString("en-US")} events), so this server stopped queuing for it. When it
            answers, it is sent the latest event of each room it is behind in and fetches the rest
            itself. Nothing is lost.
          </dd>
        </div>
        <div className="contents">
          <dt className="text-text">Waiting to send</dt>
          <dd>
            Events (PDUs) and other messages (EDUs: typing, read receipts, presence, device updates)
            queued for the server, sent as soon as it answers.
          </dd>
        </div>
        <div className="contents">
          <dt className="text-text">Shared rooms</dt>
          <dd>
            How many rooms have users from both servers. A server sharing none is only a record:
            nothing is sent to it until a room brings the two together again, so it may be forgotten
            (its row&apos;s Forget, or the prune below), and the hourly sweep forgets it in time.
            &ldquo;No shared room&rdquo; above lists just those.
          </dd>
        </div>
        <div className="contents">
          <dt className="text-text">Order and pages</dt>
          <dd>
            Failing servers come first, then the rest by name; sort by a column to change that.
            &ldquo;Status&rdquo; sorts by when the failures began, longest first; &ldquo;Waiting to
            send&rdquo; by queued events. The list is read {DESTINATION_PAGE_SIZE} servers at a
            time, and the count under it is the whole list&apos;s.
          </dd>
        </div>
      </dl>
    </details>
  );
}
