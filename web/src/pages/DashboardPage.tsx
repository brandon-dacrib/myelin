import { useMemo, type ReactNode } from "react";
import { useNavigate } from "@tanstack/react-router";
import { CheckCircle2, TriangleAlert, CircleX } from "lucide-react";
import {
  useStatisticsOverview,
  useServerInfo,
  useClusterStatus,
  useFederationDestinations,
  useRecentAuditEntries,
} from "@/api/dashboard";
import { useAppservices, deriveDisplayName } from "@/api/bridges";
import { classifyError } from "@/api/problem";
import { Badge } from "@/components/ui/badge/Badge";
import { QueryProblemState } from "@/components/QueryProblemState";
import { SkeletonText, Skeleton } from "@/components/ui/skeleton/Skeleton";
import { RelativeTime } from "@/components/RelativeTime";
import { navigateToHref } from "@/lib/navigate-href";
import { bridgeHealthMeta, healthKeyOf } from "@/lib/bridge-state";

interface AttentionRow {
  id: string;
  severity: "danger" | "warning";
  summary: string;
  actionLabel: string;
  actionHref: string;
}

/**
 * `/` — "is the server fine right now? what needs my attention?"
 * (docs/design/information-architecture.md, Overview). Composed client-side
 * from several real endpoints; see api/dashboard.ts's doc comment for why
 * there is no single `/overview` call.
 */
export function DashboardPage() {
  const navigate = useNavigate();
  const stats = useStatisticsOverview();
  const server = useServerInfo();
  const cluster = useClusterStatus();
  const appservices = useAppservices({ limit: 50 });
  const federation = useFederationDestinations(50);
  const auditLog = useRecentAuditEntries(5);

  const isLoading =
    stats.isLoading ||
    server.isLoading ||
    cluster.isLoading ||
    appservices.isLoading ||
    federation.isLoading;

  // Deliberately no single page-wide `isError` gate: against a real server, `/server` (this
  // page's uptime/version tiles) may well answer while `/statistics/overview`, `/cluster`,
  // `/appservices` and `/federation/destinations` all still 501 (only /me, /server,
  // /server/health, /users and /users/{id} are real as of this writing). Each section below
  // renders what it has and reports its own gap honestly instead of the whole page going blank
  // because one of six independent queries failed (docs/status/16-management-web-interface.md,
  // "Degrade honestly").
  const attentionSourcesFailed = stats.isError && appservices.isError && federation.isError;

  const unhealthyBridges = useMemo(
    () => (appservices.data?.items ?? []).filter((b) => b.health !== "healthy" && !b.paused),
    [appservices.data],
  );
  const failingDestinations = useMemo(
    () => (federation.data?.items ?? []).filter((d) => d.failing_since),
    [federation.data],
  );

  const attention: AttentionRow[] = useMemo(() => {
    const rows: AttentionRow[] = [];
    for (const b of unhealthyBridges) {
      rows.push({
        id: `bridge-${b.id}`,
        severity: b.health === "down" ? "danger" : "warning",
        summary: `Bridge ${deriveDisplayName(b)} is ${bridgeHealthMeta[healthKeyOf(b)].label.toLowerCase()}.`,
        actionLabel: "Open bridge",
        actionHref: `/bridges/${b.id}`,
      });
    }
    for (const d of failingDestinations) {
      if (!d.failing_since) continue;
      // Date.now() is intentionally impure here, same as RelativeTime: "has
      // this been failing over an hour" must read the current time. This
      // memo re-runs on every federation poll (30s), a fine enough clock.
      // eslint-disable-next-line react-hooks/purity -- see comment above
      const nowMs = Date.now();
      const hourOld = nowMs - new Date(d.failing_since).getTime() > 3_600_000;
      if (!hourOld) continue;
      rows.push({
        id: `destination-${d.server_name}`,
        severity: "warning",
        summary: `Federation with ${d.server_name} has been failing for over an hour.`,
        actionLabel: "Open federation",
        actionHref: "/federation",
      });
    }
    if ((stats.data?.pending_reports_count ?? 0) > 0) {
      rows.push({
        id: "pending-reports",
        severity: "warning",
        summary: `${stats.data!.pending_reports_count} report${stats.data!.pending_reports_count === 1 ? "" : "s"} awaiting action.`,
        actionLabel: "Open reports",
        actionHref: "/reports",
      });
    }
    return rows;
  }, [unhealthyBridges, failingDestinations, stats.data]);

  const singleNode = (cluster.data?.replica_count ?? 1) <= 1;

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <h1 className="text-xl text-text">Overview</h1>

      <div className="mt-6 flex flex-col gap-8">
        {/* Attention */}
        <section aria-labelledby="attention-heading">
          <h2 id="attention-heading" className="text-md font-medium text-text">
            Attention
          </h2>
          <div className="mt-3 rounded-md border border-border bg-surface">
            {isLoading && (
              <div className="p-4">
                <SkeletonText lines={3} />
              </div>
            )}
            {!isLoading && attentionSourcesFailed && (
              <QueryProblemState
                error={stats.error ?? appservices.error ?? federation.error}
                resource="what needs attention"
                compact
                onRetry={() => {
                  stats.refetch();
                  appservices.refetch();
                  federation.refetch();
                }}
              />
            )}
            {!isLoading && !attentionSourcesFailed && attention.length === 0 && (
              <p className="flex items-center gap-2 p-4 text-sm text-text-muted">
                <CheckCircle2 size={16} aria-hidden="true" className="text-success" />
                Nothing needs your attention.
              </p>
            )}
            {!isLoading &&
              !attentionSourcesFailed &&
              attention.map((item, i) => {
                const Icon = item.severity === "danger" ? CircleX : TriangleAlert;
                return (
                  <div
                    key={item.id}
                    className={
                      "flex items-center gap-3 px-4 py-3" +
                      (i < attention.length - 1 ? " border-b border-border" : "")
                    }
                  >
                    <Icon
                      size={16}
                      aria-hidden="true"
                      className={item.severity === "danger" ? "text-danger" : "text-warning"}
                    />
                    <p className="flex-1 text-sm text-text">{item.summary}</p>
                    <button
                      type="button"
                      onClick={() => navigateToHref(navigate, item.actionHref)}
                      className="rounded-sm px-2 py-1 text-sm font-medium text-accent hover:underline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
                    >
                      {item.actionLabel}
                    </button>
                  </div>
                );
              })}
          </div>
        </section>

        {/* Health tiles */}
        <section aria-labelledby="health-heading">
          <h2 id="health-heading" className="text-md font-medium text-text">
            Health
          </h2>
          <div className="mt-3 grid grid-cols-2 gap-3 sm:grid-cols-3 xl:grid-cols-4">
            {isLoading &&
              Array.from({ length: 6 }).map((_, i) => (
                <Skeleton key={i} className="h-20 rounded-md" />
              ))}
            {!isLoading && (
              <>
                <Tile
                  label="Version"
                  value={
                    server.isError ? (
                      <TileProblem error={server.error} />
                    ) : (
                      (server.data?.version ?? "—")
                    )
                  }
                />
                <Tile
                  label="Uptime"
                  value={
                    server.isError ? (
                      <TileProblem error={server.error} />
                    ) : server.data?.uptime_ms != null ? (
                      formatUptime(server.data.uptime_ms)
                    ) : (
                      "—"
                    )
                  }
                />
                <Tile
                  label="Mode"
                  value={
                    cluster.isError ? (
                      <TileProblem error={cluster.error} />
                    ) : singleNode ? (
                      "Single node"
                    ) : (
                      "Cluster"
                    )
                  }
                />
                <Tile
                  label="Users"
                  value={
                    stats.isError ? (
                      <TileProblem error={stats.error} />
                    ) : (
                      (stats.data?.users_count ?? 0).toLocaleString()
                    )
                  }
                />
                <Tile
                  label="Rooms"
                  value={
                    stats.isError ? (
                      <TileProblem error={stats.error} />
                    ) : (
                      (stats.data?.rooms_count ?? 0).toLocaleString()
                    )
                  }
                />
                <Tile
                  label="Daily active users"
                  value={
                    stats.isError ? (
                      <TileProblem error={stats.error} />
                    ) : (
                      (stats.data?.daily_active_users ?? 0).toLocaleString()
                    )
                  }
                />
              </>
            )}
          </div>
        </section>

        <div className="grid grid-cols-1 gap-8 xl:grid-cols-2">
          {/* Bridges strip */}
          <section aria-labelledby="bridges-strip-heading">
            <h2 id="bridges-strip-heading" className="text-md font-medium text-text">
              Bridges
            </h2>
            <ul className="mt-3 flex flex-col gap-2">
              {!isLoading && appservices.isError && (
                <li>
                  <QueryProblemState
                    error={appservices.error}
                    resource="bridges"
                    scope="bridges:read"
                    compact
                    onRetry={() => appservices.refetch()}
                  />
                </li>
              )}
              {!isLoading &&
                !appservices.isError &&
                appservices.data?.items.map((b) => {
                  const meta = bridgeHealthMeta[healthKeyOf(b)];
                  return (
                    <li key={b.id}>
                      <button
                        type="button"
                        onClick={() =>
                          navigate({ to: "/bridges/$bridgeId", params: { bridgeId: b.id ?? "" } })
                        }
                        className="flex w-full items-center justify-between gap-3 rounded-md border border-border bg-surface px-4 py-2.5 text-left hover:bg-surface-sunken focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
                      >
                        <span className="text-sm font-medium text-text">
                          {deriveDisplayName(b)}
                        </span>
                        <Badge status={meta.status}>{meta.label}</Badge>
                      </button>
                    </li>
                  );
                })}
              {!isLoading && !appservices.isError && appservices.data?.items.length === 0 && (
                <p className="text-sm text-text-muted">No bridges yet.</p>
              )}
            </ul>
          </section>

          {/* Federation strip */}
          <section aria-labelledby="federation-strip-heading">
            <h2 id="federation-strip-heading" className="text-md font-medium text-text">
              Federation
            </h2>
            <div className="mt-3 flex gap-3">
              {!isLoading && federation.isError && (
                <QueryProblemState
                  error={federation.error}
                  resource="federation destinations"
                  scope="admin:read"
                  compact
                  onRetry={() => federation.refetch()}
                  className="w-full"
                />
              )}
              {!isLoading &&
                !federation.isError &&
                (
                  [
                    {
                      label: "Healthy",
                      status: "success" as const,
                      count: (federation.data?.items ?? []).filter(
                        (d) => !d.failing_since && !d.retry_interval_ms,
                      ).length,
                    },
                    {
                      label: "Backing off",
                      status: "warning" as const,
                      count: (federation.data?.items ?? []).filter(
                        (d) => !d.failing_since && d.retry_interval_ms,
                      ).length,
                    },
                    {
                      label: "Failing",
                      status: "danger" as const,
                      count: (federation.data?.items ?? []).filter((d) => d.failing_since).length,
                    },
                  ] as const
                ).map((tile) => (
                  <button
                    key={tile.label}
                    type="button"
                    onClick={() => navigate({ to: "/federation" })}
                    className="flex-1 rounded-md border border-border bg-surface p-4 text-left hover:bg-surface-sunken focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
                  >
                    <p className="text-2xl text-text">{tile.count}</p>
                    <Badge status={tile.status} className="mt-1">
                      {tile.label}
                    </Badge>
                  </button>
                ))}
            </div>
          </section>
        </div>

        {/* Recent audit */}
        <section aria-labelledby="audit-heading">
          <h2 id="audit-heading" className="text-md font-medium text-text">
            Recent audit
          </h2>
          <ul className="mt-3 divide-y divide-border rounded-md border border-border bg-surface">
            {!isLoading && auditLog.isError && (
              <li>
                <QueryProblemState
                  error={auditLog.error}
                  resource="the audit log"
                  scope="admin:read"
                  compact
                  onRetry={() => auditLog.refetch()}
                />
              </li>
            )}
            {!isLoading &&
              !auditLog.isError &&
              auditLog.data?.items.map((entry) => (
                <li
                  key={entry.id}
                  className="flex items-center justify-between gap-3 px-4 py-2.5 text-sm"
                >
                  <span className="text-text">
                    {entry.action} &middot; {entry.target.type} {entry.target.id}
                  </span>
                  <span className="flex items-center gap-3 text-text-muted">
                    <span className="font-identifier">
                      {entry.actor.display_name ?? entry.actor.id}
                    </span>
                    <RelativeTime at={entry.recorded_at} />
                  </span>
                </li>
              ))}
            {!isLoading && !auditLog.isError && (auditLog.data?.items.length ?? 0) === 0 && (
              <li className="px-4 py-2.5 text-sm text-text-muted">
                No changes recorded yet. Every action taken here is logged.
              </li>
            )}
          </ul>
        </section>
      </div>
    </div>
  );
}

function Tile({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="rounded-md border border-border bg-surface p-4">
      <p className="text-xs text-text-muted">{label}</p>
      <div className="mt-1 text-2xl text-text">{value}</div>
    </div>
  );
}

/** A tile-sized "not implemented"/"not available" marker, for a health tile whose one source
 * query failed while the rest of the dashboard loaded fine. */
function TileProblem({ error }: { error: unknown }) {
  const { kind } = classifyError(error);
  const label =
    kind === "not-implemented"
      ? "Not implemented"
      : kind === "unavailable"
        ? "Unavailable"
        : "Unknown";
  return <span className="text-sm font-normal text-text-faint">{label}</span>;
}

function formatUptime(ms: number): string {
  const hours = Math.floor(ms / 3_600_000);
  const days = Math.floor(hours / 24);
  const remHours = hours % 24;
  return days > 0 ? `${days}d ${remHours}h` : `${hours}h`;
}
