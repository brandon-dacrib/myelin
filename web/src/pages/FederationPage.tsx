import { useMemo } from "react";
import { useNavigate, Link } from "@tanstack/react-router";
import { Globe } from "lucide-react";
import { useFederationDestinations } from "@/api/dashboard";
import type { Destination } from "@/api/federation";
import { Badge } from "@/components/ui/badge/Badge";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { hasScope } from "@/lib/auth";
import { destinationHealth, destinationSeverity } from "@/lib/federation";
import { useFederationQueueLimit } from "@/api/federation";
import { CatchUpBadge } from "./federation/CatchUp";
import { OwnKeysPanel } from "./federation/FederationPanels";

/** `/federation` — flows.md flow 4: watch federation health. */
export function FederationPage() {
  const navigate = useNavigate();
  const canRead = hasScope("admin:read");
  const { data, isLoading, isError, error, refetch } = useFederationDestinations(50);

  const rows = useMemo(() => {
    const items = data?.items ?? [];
    // Attention first: failing, then catching up or backing off, then healthy.
    return [...items].sort((a, b) => destinationSeverity(a) - destinationSeverity(b));
  }, [data]);
  const catchingUp = rows.filter((d) => d.catch_up_since).length;

  const columns: Column<Destination>[] = [
    {
      key: "server_name",
      header: "Server",
      priority: 1,
      interactive: true,
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
      key: "status",
      header: "Status",
      priority: 1,
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
      render: (d) => <RelativeTime at={d.last_successful_at} />,
    },
    {
      key: "pending",
      header: "Waiting to send",
      priority: 3,
      align: "end",
      render: (d) =>
        d.catch_up_since ? (
          <span className="text-text-muted">not queued</span>
        ) : (
          (d.pending_pdu_count ?? 0) + (d.pending_edu_count ?? 0)
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

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <h1 className="text-xl text-text">Federation</h1>
      <p className="mt-1 max-w-3xl text-sm text-text-muted">
        The other Matrix servers this one sends to: every server with a user in a room your users
        are in. Each row says whether sending to it works now; open one for its shared rooms, its
        signing keys and its retry state.
      </p>

      <StatusKey catchingUp={catchingUp} />

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
          <DataTable
            caption="Federation destinations"
            columns={columns}
            rows={rows}
            getRowId={(d) => d.server_name ?? ""}
            loading={isLoading}
            onRowClick={(d) =>
              navigate({
                to: "/federation/$serverName",
                params: { serverName: d.server_name ?? "" },
              })
            }
            empty={
              <EmptyState
                icon={<Globe aria-hidden="true" />}
                title="No federation traffic yet"
                description="When your users join rooms on other servers, those servers appear here."
              />
            }
          />
        </div>
      )}

      <OwnKeysPanel />
    </div>
  );
}

/**
 * What each status means, once, above the table: an operator should not need the docs to read
 * a badge. Catch-up is explained in full when a destination is in it.
 */
function StatusKey({ catchingUp }: { catchingUp: number }) {
  const { limit } = useFederationQueueLimit();
  return (
    <details className="mt-3 max-w-3xl text-sm text-text-muted" open={catchingUp > 0}>
      <summary className="cursor-pointer text-accent hover:underline">
        What the statuses mean
        {catchingUp > 0 &&
          ` (${catchingUp} ${catchingUp === 1 ? "server is" : "servers are"} catching up)`}
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
      </dl>
    </details>
  );
}
