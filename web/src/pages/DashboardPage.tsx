import { useMemo, type ReactNode } from "react";
import { formatBytes, formatCount, formatUptime, joinWithOr } from "@/lib/format";
import { Link, useNavigate } from "@tanstack/react-router";
import {
  CheckCircle2,
  TriangleAlert,
  CircleX,
  Info,
  UserPlus,
  Cable,
  ArrowRightLeft,
  KeyRound,
} from "lucide-react";
import {
  useStatisticsOverview,
  useServerInfo,
  useClusterStatus,
  useFederationDestinations,
  useRecentAuditEntries,
  useServerHealth,
} from "@/api/dashboard";
import {
  useAppservices,
  useBridgeOfferings,
  deriveDisplayName,
  isBuiltInAppservice,
} from "@/api/bridges";
import { useMigration } from "@/api/migration";
import { useTasks } from "@/api/tasks";
import { isCounter, useTimeseries, type Metric } from "@/api/statistics";
import { Sparkline } from "@/components/Sparkline";
import { hasScope } from "@/lib/auth";
import { describeTaskAction } from "@/lib/tasks";
import { describeAction, targetTypeLabel } from "@/lib/audit";
import { classifyError } from "@/api/problem";
import { Badge } from "@/components/ui/badge/Badge";
import { QueryProblemState } from "@/components/QueryProblemState";
import { SkeletonText, Skeleton } from "@/components/ui/skeleton/Skeleton";
import { RelativeTime } from "@/components/RelativeTime";
import { navigateToHref } from "@/lib/navigate-href";
import { bridgeHealthMeta, healthKeyOf } from "@/lib/bridge-state";
import { healthSummary } from "@/lib/server-health";
import { ServerHealthCard } from "./dashboard/ServerHealthCard";

/** Failing servers listed one row each under Attention; more than this are one row. */
const HOUR_OLD_ROWS = 3;

interface AttentionRow {
  id: string;
  severity: "danger" | "warning";
  summary: string;
  actionLabel: string;
  actionHref: string;
  /** Search parameters for the href (a filtered list). */
  actionSearch?: Record<string, string>;
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
  // The failing destinations, longest failing first, for the attention rows, with how many
  // there are in all; and how many are not failing. The counts shown are the server's totals
  // (`include_total`), never the length of a page.
  const failing = useFederationDestinations({
    failing: true,
    sort: "failing_since",
    limit: 50,
    include_total: true,
  });
  const notFailing = useFederationDestinations({ failing: false, limit: 1, include_total: true });
  const auditLog = useRecentAuditEntries(5);
  const failedTasks = useTasks({ status: "failed", limit: 20 });
  const health = useServerHealth();
  // For "Get started": a server with nobody but its administrator, nothing offered and no import
  // under way is one the operator has just set up, and the page says what to do first.
  const offerings = useBridgeOfferings();
  const migration = useMigration();

  const isLoading =
    stats.isLoading ||
    server.isLoading ||
    cluster.isLoading ||
    appservices.isLoading ||
    failing.isLoading;

  // Deliberately no single page-wide `isError` gate: against a real server, `/server` (this
  // page's uptime/version tiles) may well answer while `/statistics/overview`, `/cluster`,
  // `/appservices` and `/federation/destinations` all still 501 (only /me, /server,
  // /server/health, /users and /users/{id} are real as of this writing). Each section below
  // renders what it has and reports its own gap honestly instead of the whole page going blank
  // because one of six independent queries failed (docs/status/16-management-web-interface.md,
  // "Degrade honestly").
  const attentionSourcesFailed = stats.isError && appservices.isError && failing.isError;

  // What an all-clear below cannot vouch for, because the source that would have said so could
  // not be asked. Without this, "Nothing needs your attention" is shown over a server whose
  // bridges and federation answered 501 -- an all-clear about things nobody looked at.
  const unchecked: string[] = [];
  if (appservices.isError) unchecked.push("bridges");
  if (failing.isError) unchecked.push("federation");
  if (stats.isError || stats.data?.pending_reports_count == null) unchecked.push("reports");
  if (failedTasks.isError) unchecked.push("tasks");
  if (health.isError) unchecked.push("server health");

  // The server's own bridge manager is a registration too, and its health is "unknown" because
  // nothing probes it: that is not a bridge in trouble, so it is not under Attention.
  const bridges = useMemo(
    () => (appservices.data?.items ?? []).filter((b) => !isBuiltInAppservice(b)),
    [appservices.data],
  );
  const unhealthyBridges = useMemo(
    () => bridges.filter((b) => b.health !== "healthy" && !b.paused),
    [bridges],
  );
  const failingDestinations = useMemo(
    () => (failing.data?.items ?? []).filter((d) => d.failing_since),
    [failing.data],
  );
  const failingMore = Boolean(failing.data?.next_cursor);

  const attention: AttentionRow[] = useMemo(() => {
    const rows: AttentionRow[] = [];
    if (health.data && health.data.status && health.data.status !== "ok") {
      rows.push({
        id: "server-health",
        severity: health.data.status === "down" ? "danger" : "warning",
        summary: healthSummary(health.data.status, health.data.checks),
        actionLabel: "See the checks",
        actionHref: "#health-heading",
      });
    }
    for (const b of unhealthyBridges) {
      rows.push({
        id: `bridge-${b.id}`,
        severity: b.health === "down" ? "danger" : "warning",
        summary: `Bridge ${deriveDisplayName(b)} is ${bridgeHealthMeta[healthKeyOf(b)].label.toLowerCase()}.`,
        actionLabel: "Open bridge",
        actionHref: `/bridges/${b.id}`,
      });
    }
    // Date.now() is intentionally impure here, same as RelativeTime: "has this been failing
    // over an hour" must read the current time. This memo re-runs on every federation poll
    // (30s), a fine enough clock.
    // eslint-disable-next-line react-hooks/purity -- see comment above
    const nowMs = Date.now();
    // The page is longest failing first, so the hour-old ones are its start.
    const hourOld = failingDestinations.filter(
      (d) => d.failing_since && nowMs - new Date(d.failing_since).getTime() > 3_600_000,
    );
    if (hourOld.length <= HOUR_OLD_ROWS) {
      for (const d of hourOld) {
        rows.push({
          id: `destination-${d.server_name}`,
          severity: "warning",
          summary: `Federation with ${d.server_name} has been failing for over an hour.`,
          actionLabel: `Open ${d.server_name}`,
          actionHref: `/federation/${encodeURIComponent(d.server_name ?? "")}`,
        });
      }
    } else {
      // Every one on the page, and more pages behind it: there may be more than it holds.
      const atLeast = hourOld.length === failingDestinations.length && failingMore;
      rows.push({
        id: "destinations-hour-old",
        severity: "warning",
        summary: `${atLeast ? "At least " : ""}${hourOld.length} servers have been failing for over an hour.`,
        actionLabel: "See the failing servers",
        actionHref: "/federation",
        actionSearch: { show: "failing" },
      });
    }
    for (const task of failedTasks.data?.items ?? []) {
      // Same deliberate clock read as above: "failed in the last day" is about now.
      // eslint-disable-next-line react-hooks/purity -- see comment above
      const nowMs = Date.now();
      const endedAt = Date.parse(task.finished_at ?? task.created_at);
      if (nowMs - endedAt > 24 * 3_600_000) continue;
      rows.push({
        id: `task-${task.id}`,
        severity: "danger",
        summary: `Task "${describeTaskAction(task.action)}" failed${task.error?.detail ? `: ${task.error.detail}` : "."}`,
        actionLabel: "Open task",
        actionHref: `/tasks/${task.id}`,
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
  }, [
    health.data,
    unhealthyBridges,
    failingDestinations,
    failingMore,
    stats.data,
    failedTasks.data,
  ]);

  const singleNode = (cluster.data?.replica_count ?? 1) <= 1;
  const justSetUp =
    stats.data?.users_count != null &&
    stats.data.users_count <= 1 &&
    (offerings.data?.length ?? 0) === 0 &&
    (migration.data?.status == null || migration.data.status === "idle");

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <h1 className="text-xl text-text">Overview</h1>
      <ServerLine
        name={server.data?.name}
        version={server.data?.version}
        uptimeMs={server.data?.uptime_ms}
        mode={cluster.isError ? undefined : singleNode ? "single" : "cluster"}
        replicas={cluster.data?.replica_count}
        loading={server.isLoading || cluster.isLoading}
      />

      <div className="mt-6 flex flex-col gap-8">
        {justSetUp && <GetStarted />}

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
                error={stats.error ?? appservices.error ?? failing.error}
                resource="what needs attention"
                compact
                onRetry={() => {
                  stats.refetch();
                  appservices.refetch();
                  failing.refetch();
                }}
              />
            )}
            {!isLoading &&
              !attentionSourcesFailed &&
              attention.length === 0 &&
              (unchecked.length === 0 ? (
                <p className="flex items-center gap-2 p-4 text-sm text-text-muted">
                  <CheckCircle2 size={16} aria-hidden="true" className="text-success" />
                  Nothing needs your attention.
                </p>
              ) : (
                <p className="flex items-center gap-2 p-4 text-sm text-text-muted">
                  <Info size={16} aria-hidden="true" className="shrink-0 text-text-faint" />
                  Nothing needs your attention, as far as this server can tell. It can&apos;t check{" "}
                  {joinWithOr(unchecked)} yet.
                </p>
              ))}
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
                      onClick={() =>
                        item.actionHref.startsWith("#")
                          ? document
                              .getElementById(item.actionHref.slice(1))
                              ?.scrollIntoView({ block: "start" })
                          : navigateToHref(navigate, item.actionHref, item.actionSearch)
                      }
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
          <div className="mt-3">
            <ServerHealthCard />
          </div>
          <div className="mt-3 grid grid-cols-2 gap-3 sm:grid-cols-3">
            {isLoading &&
              Array.from({ length: 3 }).map((_, i) => (
                <Skeleton key={i} className="h-20 rounded-md" />
              ))}
            {!isLoading && (
              <>
                <Tile
                  label="Users"
                  value={
                    stats.isError ? (
                      <TileProblem error={stats.error} />
                    ) : (
                      formatCount(stats.data?.users_count)
                    )
                  }
                />
                <Tile
                  label="Rooms"
                  value={
                    stats.isError ? (
                      <TileProblem error={stats.error} />
                    ) : (
                      formatCount(stats.data?.rooms_count)
                    )
                  }
                />
                <Tile
                  label="Daily active users"
                  value={
                    stats.isError ? (
                      <TileProblem error={stats.error} />
                    ) : (
                      formatCount(stats.data?.daily_active_users)
                    )
                  }
                  hint="People who used the server in the last 24 hours."
                />
              </>
            )}
          </div>
        </section>

        {hasScope("admin:read") && <ActivityStrip />}

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
                bridges.map((b) => {
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
              {!isLoading && !appservices.isError && bridges.length === 0 && (
                <li className="text-sm text-text-muted">
                  No bridges yet.{" "}
                  <Link
                    to="/bridges/new"
                    className="text-accent underline underline-offset-2 hover:no-underline"
                  >
                    Offer one
                  </Link>{" "}
                  to let people here reach WhatsApp, Signal, Telegram and more.
                </li>
              )}
            </ul>
          </section>

          {/* Federation strip */}
          <FederationStrip
            loading={isLoading}
            failingCount={stats.data?.federation_destinations_failing_count ?? failing.data?.total}
            notFailingCount={notFailing.data?.total}
            errors={{
              failing: stats.isError && failing.isError ? (stats.error ?? failing.error) : null,
              notFailing: notFailing.isError ? notFailing.error : null,
            }}
            onRetry={() => {
              stats.refetch();
              failing.refetch();
              notFailing.refetch();
            }}
          />
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
                  <Link
                    to="/audit/$entryId"
                    params={{ entryId: entry.id }}
                    className="text-text hover:text-accent hover:underline"
                  >
                    {describeAction(entry.action)} &middot; {targetTypeLabel(entry.target.type)}{" "}
                    {entry.target.id}
                  </Link>
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

/**
 * How federation is going, in two numbers that are the server's own totals: how many
 * destinations are failing (`federation_destinations_failing_count`, with the failing list's
 * `total` as a stand-in when the Overview's counts cannot be read) and how many are not (the
 * `failing=false` list's `total`). Each opens the Federation page filtered to those servers.
 * Not one subtracted from the other: the Overview's counts are a snapshot the server recounts
 * about once a minute, and a difference of a snapshot and a live count can be neither.
 */
function FederationStrip({
  loading,
  failingCount,
  notFailingCount,
  errors,
  onRetry,
}: {
  loading: boolean;
  failingCount: number | undefined;
  notFailingCount: number | undefined;
  errors: { failing: unknown; notFailing: unknown };
  onRetry: () => void;
}) {
  const navigate = useNavigate();
  const tiles = [
    {
      label: "Failing",
      status: "danger" as const,
      count: failingCount,
      error: errors.failing,
      show: "failing" as const,
    },
    {
      label: "Not failing",
      status: "success" as const,
      count: notFailingCount,
      error: errors.notFailing,
      show: "not-failing" as const,
    },
  ];
  return (
    <section aria-labelledby="federation-strip-heading">
      <h2 id="federation-strip-heading" className="text-md font-medium text-text">
        Federation
      </h2>
      {!loading && errors.failing && errors.notFailing ? (
        <div className="mt-3">
          <QueryProblemState
            error={errors.failing}
            resource="federation destinations"
            scope="admin:read"
            compact
            onRetry={onRetry}
            className="w-full"
          />
        </div>
      ) : (
        <>
          <div className="mt-3 flex gap-3">
            {!loading &&
              tiles.map((tile) => (
                <button
                  key={tile.label}
                  type="button"
                  onClick={() => navigate({ to: "/federation", search: { show: tile.show } })}
                  className="flex-1 rounded-md border border-border bg-surface p-4 text-left hover:bg-surface-sunken focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
                >
                  <p className="text-2xl text-text tabular-nums">
                    {tile.count != null ? (
                      formatCount(tile.count)
                    ) : tile.error ? (
                      <TileProblem error={tile.error} />
                    ) : (
                      "—"
                    )}
                  </p>
                  <Badge status={tile.status} className="mt-1">
                    {tile.label}
                  </Badge>
                </button>
              ))}
          </div>
          {!loading && (
            <p className="mt-2 text-xs text-text-muted">
              Every server this one has tried to reach, counted by the server. Failing means its
              requests are failing now; the failing count is refreshed about once a minute.
            </p>
          )}
        </>
      )}
    </section>
  );
}

const ACTIVITY: { metric: Metric; label: string; format: (value: number) => string }[] = [
  { metric: "daily_active_users", label: "Daily active, 7-day trend", format: formatCount },
  { metric: "users.registered", label: "New accounts, 7 days", format: formatCount },
  { metric: "media.uploaded_bytes", label: "Media uploaded, 7 days", format: formatBytes },
  { metric: "reports.received", label: "Reports received, 7 days", format: formatCount },
];

/**
 * The last seven days at a glance (`GET /statistics/timeseries`): a sparkline and one number per
 * metric -- the total for a counter, the latest sample for a gauge. Each links to Statistics.
 */
function ActivityStrip() {
  return (
    <section aria-labelledby="activity-heading">
      <div className="flex items-baseline justify-between gap-3">
        <h2 id="activity-heading" className="text-md font-medium text-text">
          Activity
        </h2>
        <Link to="/statistics" className="text-sm text-accent hover:underline">
          All statistics
        </Link>
      </div>
      <div className="mt-3 grid grid-cols-2 gap-3 xl:grid-cols-4">
        {ACTIVITY.map((item) => (
          <ActivityTile key={item.metric} {...item} />
        ))}
      </div>
    </section>
  );
}

function ActivityTile({
  metric,
  label,
  format,
}: {
  metric: Metric;
  label: string;
  format: (value: number) => string;
}) {
  const series = useTimeseries(metric, "7d");
  const points = (series.data?.points ?? []).map((p) => ({ value: p.value ?? 0 }));
  const value = isCounter(metric)
    ? points.reduce((sum, p) => sum + p.value, 0)
    : points[points.length - 1]?.value;
  return (
    <Link
      to="/statistics"
      className="block rounded-md border border-border bg-surface p-4 hover:bg-surface-sunken focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
    >
      <p className="text-xs text-text-muted">{label}</p>
      {series.isLoading ? (
        <Skeleton className="mt-2 h-12" />
      ) : (
        <>
          <div className="mt-1 text-2xl text-text tabular-nums">
            {series.isError ? (
              <TileProblem error={series.error} />
            ) : value == null ? (
              "—"
            ) : (
              format(value)
            )}
          </div>
          {!series.isError && points.length > 1 && (
            <Sparkline points={points} className="mt-2 h-7 w-full text-accent" />
          )}
        </>
      )}
    </Link>
  );
}

/**
 * One line under the title saying what this server is: its name, version, whether it is one
 * process or a cluster, and how long it has been up. What the Health tiles used to spend three
 * tiles on, in the words an operator would use to describe the server to somebody else.
 */
function ServerLine({
  name,
  version,
  uptimeMs,
  mode,
  replicas,
  loading,
}: {
  name?: string;
  version?: string;
  uptimeMs?: number;
  mode?: "single" | "cluster";
  replicas?: number;
  loading: boolean;
}) {
  if (loading) return <Skeleton className="mt-1 h-5 w-96 max-w-full" />;
  const parts: ReactNode[] = [];
  if (name) parts.push(<span className="font-identifier text-text">{name}</span>);
  if (version) parts.push(<>version {version}</>);
  if (mode === "single") parts.push(<>one process, which serves everything</>);
  if (mode === "cluster")
    parts.push(
      <>
        a cluster of {replicas} replicas (
        <Link to="/cluster" className="text-accent underline underline-offset-2 hover:no-underline">
          see Cluster
        </Link>
        )
      </>,
    );
  if (uptimeMs != null) parts.push(<>up {formatUptime(uptimeMs)}</>);
  if (parts.length === 0) return null;
  return (
    <p className="mt-1 text-sm text-text-muted" data-testid="server-line">
      {parts.map((part, i) => (
        <span key={i}>
          {i > 0 && <span aria-hidden="true"> &middot; </span>}
          {part}
        </span>
      ))}
    </p>
  );
}

const GET_STARTED: {
  icon: typeof UserPlus;
  title: string;
  text: string;
  href: string;
  scope?: Parameters<typeof hasScope>[0];
}[] = [
  {
    icon: UserPlus,
    title: "Add people",
    text: "Create an account for somebody, or make an invite link they use to pick their own username and password.",
    href: "/users",
    scope: "admin:read",
  },
  {
    icon: KeyRound,
    title: "Let people sign up themselves",
    text: "Registration is off until you turn it on, so nobody can create an account here without you.",
    href: "/configuration/auth",
    scope: "admin:read",
  },
  {
    icon: Cable,
    title: "Offer a bridge",
    text: "Connect WhatsApp, Signal, Telegram, Discord and more; each person then gets their own bridge by messaging its bot.",
    href: "/bridges/new",
    scope: "bridges:read",
  },
  {
    icon: ArrowRightLeft,
    title: "Move here from Synapse",
    text: "Copy accounts, rooms, keys and media from a Synapse while it keeps running, and cut over when you are ready.",
    href: "/migration",
    scope: "admin:read",
  },
];

/**
 * The first things to do on a server that has only its administrator: one card per task, each a
 * sentence and a link to where it is done. Shown until the server has people, an offered bridge
 * or an import under way, whichever comes first.
 */
function GetStarted() {
  const navigate = useNavigate();
  const items = GET_STARTED.filter((item) => !item.scope || hasScope(item.scope));
  if (items.length === 0) return null;
  return (
    <section aria-labelledby="get-started-heading">
      <h2 id="get-started-heading" className="text-md font-medium text-text">
        Get started
      </h2>
      <p className="mt-1 text-sm text-text-muted">
        This server is running and has only you on it. These are the usual first steps; this section
        goes away once people are here.
      </p>
      <ul className="mt-3 grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-4">
        {items.map((item) => {
          const Icon = item.icon;
          return (
            <li key={item.href}>
              <button
                type="button"
                onClick={() => navigateToHref(navigate, item.href)}
                className="flex h-full w-full flex-col items-start gap-2 rounded-md border border-border bg-surface p-4 text-left hover:bg-surface-sunken focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]"
              >
                <span className="flex items-center gap-2 text-sm font-medium text-text">
                  <Icon size={16} aria-hidden="true" className="text-accent" />
                  {item.title}
                </span>
                <span className="text-xs text-text-muted">{item.text}</span>
              </button>
            </li>
          );
        })}
      </ul>
    </section>
  );
}

function Tile({ label, value, hint }: { label: string; value: ReactNode; hint?: string }) {
  return (
    <div className="rounded-md border border-border bg-surface p-4">
      <p className="text-xs text-text-muted">{label}</p>
      <div className="mt-1 text-2xl text-text">{value}</div>
      {hint && <p className="mt-1 text-xs text-text-faint">{hint}</p>}
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
