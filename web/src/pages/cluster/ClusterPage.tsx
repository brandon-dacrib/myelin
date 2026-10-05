import { useState, type ReactNode } from "react";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { Boxes, Server } from "lucide-react";
import {
  heartbeatTrend,
  replicaIsMoving,
  useAllShards,
  useRefreshShardsWhenSettled,
  useReplicas,
  useShardPage,
  type Replica,
  type Shard,
  type ShardKind,
} from "@/api/cluster";
import { useClusterStatus } from "@/api/dashboard";
import { useTask } from "@/api/tasks";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { Sparkline } from "@/components/Sparkline";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { Field } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { Skeleton } from "@/components/ui/skeleton/Skeleton";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { hasScope } from "@/lib/auth";
import { cn } from "@/lib/cn";
import {
  describeGeneration,
  drainBlockedReason,
  isDrainRequested,
  isSingleNode,
  plural,
  REPLICA_STATUS_META,
  replicaStatusMeta,
  SHARD_KIND_LABELS,
  SHARD_KINDS,
  type ReplicaStatus,
} from "@/lib/cluster";
import { describeProgress, progressFraction } from "@/lib/tasks";
import { TaskProgressBar } from "@/pages/tasks/TaskProgress";
import type { ClusterSearch, ShardView } from "./cluster-search";
import { ReplicaActionDialog, type PendingReplicaAction } from "./ReplicaActionDialog";
import { ShardMap } from "./ShardMap";

const SHARD_PAGE_SIZE = 50;

/**
 * `/cluster`: which replicas serve this server, which shards each owns, and draining one so it
 * can be taken down without its work stopping. A single node is a cluster of one; the page says
 * so and explains why there is nothing to drain it to.
 */
export function ClusterPage() {
  const search = useSearch({ from: "/cluster" });
  const navigate = useNavigate({ from: "/cluster" });
  const [pending, setPending] = useState<PendingReplicaAction | null>(null);
  const status = useClusterStatus();
  const replicas = useReplicas();
  const moving = Boolean(replicas.data?.some(replicaIsMoving));
  const allShards = useAllShards({ moving });
  useRefreshShardsWhenSettled(moving);

  if (!hasScope("admin:read")) {
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Cluster</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );
  }

  const replicaList = replicas.data ?? [];
  const singleNode = isSingleNode(status.data?.mode, replicas.data);
  const known = Boolean(status.data || replicas.data);

  return (
    <div className="mx-auto max-w-[90rem] space-y-8 p-6">
      <div>
        <h1 className="text-xl text-text">Cluster</h1>
        <p className="mt-1 text-sm text-text-muted">
          {!known
            ? "The replicas serving this server, and the shards each one owns."
            : singleNode
              ? "This server runs as a single node: one replica that owns every shard."
              : "The replicas serving this server share its work as shards: rooms, users, federation destinations and appservices, each hashed onto one owner."}
        </p>
      </div>

      <Summary
        singleNode={singleNode}
        statusKnown={Boolean(status.data)}
        replicas={replicas.data}
        replicaCount={status.data?.replica_count}
        shards={allShards.data}
        shardCount={status.data?.shard_count}
        heartbeatSeq={status.data?.heartbeat_seq}
        drainsReleasedAtOnce={status.data?.drain_released_at_once_count}
      />

      <section aria-labelledby="replicas-heading" className="space-y-3">
        <h2 id="replicas-heading" className="text-lg text-text">
          Replicas
        </h2>
        <details className="text-sm text-text-muted">
          <summary className="cursor-pointer text-accent hover:underline">
            What the columns mean
          </summary>
          <dl className="mt-2 grid max-w-4xl gap-x-4 gap-y-1.5 sm:grid-cols-[10rem_1fr]">
            <dt className="text-text">Status</dt>
            <dd>
              Joining: starting, not yet taking shards. Active: serving and owning shards. Draining:
              handing its shards to the others because an administrator asked, or it is shutting
              down. Drained: owns none, still answers requests by forwarding them. Unreachable: its
              heartbeats stopped, and the others are taking its shards.
            </dd>
            <dt className="text-text">Zone</dt>
            <dd>Where it runs, from its configuration; shards are spread across zones.</dd>
            <dt className="text-text">Last heartbeat</dt>
            <dd>
              When it last told the others it is alive. One silent for longer than the lease
              (cluster.lease_ttl) is treated as gone. The number under it (seq) goes up by one with
              every heartbeat that reaches the store, and is what the others watch. This page
              remembers the number from each poll (every 15 seconds while it is open; the server
              keeps no history of it), so it can say whether the heartbeats are still arriving: one
              whose number stops moving is about to be treated as gone. The small chart is
              heartbeats per poll since the page opened.
            </dd>
            <dt className="text-text">Drains released at once</dt>
            <dd>
              How many times, since the replica answering this page started, a drain let go of every
              shard it owned at once instead of handing them over one lease at a time: what the last
              replica does when it stops, because nobody is left to take them. Some on a server that
              was not shut down whole means a replica found itself alone.
            </dd>
            <dt className="text-text">Mesh address</dt>
            <dd>
              Where the other replicas reach it to forward requests to the shard&apos;s owner. A
              replica is named after the mesh address it advertises, so this usually says
              &ldquo;Same as its ID&rdquo; (the address is in the tooltip).
            </dd>
            <dt className="text-text">Epoch</dt>
            <dd>
              Its generation: a new one each time it starts, so a restart shows here. The server
              makes it from the clock when the replica starts, so it is shown as that time; the
              number itself is in the tooltip.
            </dd>
          </dl>
        </details>
        {singleNode && replicas.data && (
          <p
            role="note"
            id="single-node-note"
            className="rounded-md border border-info-border bg-info-bg p-3 text-sm text-text"
          >
            Draining hands a replica&apos;s shards to the other replicas, and a single node has no
            others, so there is nothing to drain it to. To drain replicas, run the server as a
            cluster (see the{" "}
            <Link
              to="/configuration/$section"
              params={{ section: "cluster" }}
              className="text-accent underline underline-offset-2 hover:no-underline"
            >
              cluster configuration
            </Link>
            ).
          </p>
        )}
        {replicas.isError ? (
          <QueryProblemState
            error={replicas.error}
            resource="replicas"
            scope="admin:read"
            onRetry={() => replicas.refetch()}
          />
        ) : (
          <ReplicasTable
            replicas={replicaList}
            loading={replicas.isLoading}
            singleNode={singleNode}
            onAction={setPending}
          />
        )}
      </section>

      <ShardsSection
        search={search}
        onSearch={(patch) => navigate({ search: { ...search, ...patch } })}
        replicas={replicaList}
        allShards={allShards}
        moving={moving}
      />

      <ReplicaActionDialog
        pending={pending}
        replicas={replicaList}
        onClose={() => setPending(null)}
      />
    </div>
  );
}

function Summary({
  singleNode,
  statusKnown,
  replicas,
  replicaCount,
  shards,
  shardCount,
  heartbeatSeq,
  drainsReleasedAtOnce,
}: {
  singleNode: boolean;
  statusKnown: boolean;
  replicas: Replica[] | undefined;
  replicaCount: number | undefined;
  shards: Shard[] | undefined;
  shardCount: number | undefined;
  heartbeatSeq: number | undefined;
  drainsReleasedAtOnce: number | undefined;
}) {
  const byStatus = new Map<ReplicaStatus, number>();
  for (const r of replicas ?? []) {
    if (r.status) byStatus.set(r.status, (byStatus.get(r.status) ?? 0) + 1);
  }
  const statusWords = (Object.keys(REPLICA_STATUS_META) as ReplicaStatus[])
    .filter((s) => byStatus.get(s))
    .map((s) => `${byStatus.get(s)} ${REPLICA_STATUS_META[s].label.toLowerCase()}`)
    .join(", ");
  const owned = shards?.filter((s) => s.owner).length;
  const total = shards?.length ?? shardCount;
  const ownerless = shards ? shards.length - (owned ?? 0) : undefined;
  const me = replicas?.find((r) => r.this_replica);
  const trend = me?.id ? heartbeatTrend(me.id) : null;
  const heartbeatNote = singleNode
    ? "A single node sends no heartbeats"
    : trend?.advancing === false && trend.lastAdvanceAt != null
      ? `This replica: no new heartbeat since ${new Date(trend.lastAdvanceAt).toLocaleTimeString()}`
      : trend?.advancing
        ? "This replica, still arriving"
        : "This replica";

  return (
    <dl className="grid gap-3 sm:grid-cols-3 xl:grid-cols-5">
      <SummaryTile
        label="Mode"
        value={!statusKnown && !replicas ? "—" : singleNode ? "Single node" : "Cluster"}
        note={singleNode ? "No mesh, no handoffs" : "Shards move between replicas"}
      />
      <SummaryTile
        label="Replicas"
        value={(replicas?.length ?? replicaCount)?.toLocaleString() ?? "—"}
        note={statusWords || undefined}
      />
      <SummaryTile
        label="Shards owned"
        value={
          owned != null && total != null
            ? `${owned.toLocaleString()} of ${total.toLocaleString()}`
            : (total?.toLocaleString() ?? "—")
        }
        note={
          ownerless == null
            ? undefined
            : ownerless === 0
              ? "Every shard has an owner"
              : `${plural(ownerless, "shard", "shards")} without an owner`
        }
      />
      <SummaryTile
        label="Heartbeat"
        // This replica's row is polled more often than `GET /cluster`; read the same number.
        value={singleNode ? "—" : ((me?.heartbeat_seq ?? heartbeatSeq)?.toLocaleString() ?? "—")}
        note={heartbeatNote}
        warn={trend?.advancing === false}
      />
      <SummaryTile
        label="Drains released at once"
        value={singleNode ? "—" : (drainsReleasedAtOnce?.toLocaleString() ?? "—")}
        note={singleNode ? "Nothing to drain on a single node" : "Since this replica started"}
      />
    </dl>
  );
}

function SummaryTile({
  label,
  value,
  note,
  warn,
}: {
  label: string;
  value: string;
  note?: string;
  /** The note is a warning (a heartbeat that stopped). */
  warn?: boolean;
}) {
  return (
    <div className="rounded-md border border-border bg-surface p-4">
      <dt className="text-xs font-medium uppercase tracking-wide text-text-muted">{label}</dt>
      <dd className="mt-1 text-2xl text-text tabular-nums">{value}</dd>
      {note && (
        <dd className={cn("mt-0.5 text-xs", warn ? "text-warning" : "text-text-muted")}>{note}</dd>
      )}
    </div>
  );
}

function ReplicasTable({
  replicas,
  loading,
  singleNode,
  onAction,
}: {
  replicas: Replica[];
  loading: boolean;
  singleNode: boolean;
  onAction: (pending: PendingReplicaAction) => void;
}) {
  const columns: Column<Replica>[] = [
    {
      key: "id",
      header: "Replica",
      render: (r) => (
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-identifier font-medium text-text">{r.id}</span>
          {r.this_replica && (
            <Badge status="info" hideIcon className="whitespace-nowrap">
              This replica
            </Badge>
          )}
          {r.role === "single-node" && (
            <Badge status="neutral" hideIcon className="whitespace-nowrap">
              Single node
            </Badge>
          )}
        </div>
      ),
      renderCompact: (r) =>
        [r.id, r.this_replica && "(this replica)", r.role === "single-node" && "single node"]
          .filter(Boolean)
          .join(" "),
    },
    {
      key: "status",
      header: "Status",
      interactive: true,
      render: (r) => <ReplicaStatusCell replica={r} />,
    },
    {
      key: "shards",
      header: "Shards",
      align: "end",
      render: (r) => (r.shard_count ?? 0).toLocaleString(),
    },
    {
      key: "zone",
      header: "Zone",
      priority: 2,
      render: (r) => (r.zone ? <span className="whitespace-nowrap">{r.zone}</span> : <Dash />),
    },
    {
      key: "heartbeat",
      header: "Last heartbeat",
      priority: 2,
      render: (r) => <HeartbeatCell replica={r} />,
      renderCompact: (r) =>
        r.last_heartbeat_at
          ? `${r.last_heartbeat_at}${r.heartbeat_seq != null ? ` (seq ${r.heartbeat_seq})` : ""}`
          : r.role === "single-node"
            ? "—"
            : "Not running",
    },
    {
      key: "version",
      header: "Version",
      priority: 3,
      render: (r) => (r.version ? <span className="font-identifier">{r.version}</span> : <Dash />),
    },
    {
      key: "mesh",
      header: "Mesh address",
      priority: 3,
      // A replica is named after the mesh address it advertises (`hs_cli::cluster`), so the
      // same string twice would only push the table past a 1280 px screen. A different address
      // (a server that names its replicas otherwise) may break anywhere.
      render: (r) =>
        !r.mesh_addr ? (
          <Dash />
        ) : r.mesh_addr === r.id ? (
          <span className="text-text-muted" title={r.mesh_addr}>
            Same as its ID
          </span>
        ) : (
          <span className="font-identifier wrap-anywhere">{r.mesh_addr}</span>
        ),
      renderCompact: (r) => r.mesh_addr ?? "—",
    },
    {
      key: "epoch",
      header: "Epoch",
      priority: 3,
      align: "end",
      // A stopped replica has no generation (the server answers 0).
      render: (r) => (isStopped(r) || !r.epoch ? <Dash /> : <GenerationCell epoch={r.epoch} />),
      renderCompact: (r) => (isStopped(r) || !r.epoch ? "—" : describeGeneration(r.epoch).title),
    },
    {
      key: "actions",
      header: "Actions",
      interactive: true,
      align: "end",
      render: (r) => (
        <ReplicaActions
          replica={r}
          replicas={replicas}
          singleNode={singleNode}
          onAction={onAction}
        />
      ),
    },
  ];

  return (
    <DataTable
      caption="Replicas"
      columns={columns}
      rows={replicas}
      getRowId={(r) => r.id ?? ""}
      loading={loading}
      empty={
        <EmptyState
          icon={<Server aria-hidden="true" />}
          title="No replicas registered"
          description="A replica appears here once it has started and announced itself."
        />
      }
    />
  );
}

/**
 * When the replica last heartbeat, the sequence number of that heartbeat, and whether the number
 * is still moving: the page's readings of it (`heartbeatTrend`) say how many heartbeats arrived
 * since the last poll, or since when none has; the sparkline is heartbeats per poll.
 */
function HeartbeatCell({ replica }: { replica: Replica }) {
  if (!replica.last_heartbeat_at) {
    return replica.role === "single-node" ? (
      <span className="text-text-faint" title="A single node sends no heartbeats">
        —
      </span>
    ) : (
      <span className="whitespace-nowrap text-text-muted">Not running</span>
    );
  }
  const trend = replica.id ? heartbeatTrend(replica.id) : null;
  const last = trend?.increments.at(-1);
  return (
    <div className="flex flex-col gap-0.5">
      <RelativeTime at={replica.last_heartbeat_at} />
      {replica.heartbeat_seq != null && (
        <span className="whitespace-nowrap text-xs text-text-muted">
          seq {replica.heartbeat_seq.toLocaleString()}
        </span>
      )}
      {trend?.advancing === true && (
        <span className="whitespace-nowrap text-xs text-text-muted">
          +{last?.toLocaleString()} since the last poll
        </span>
      )}
      {trend?.advancing === false && trend.lastAdvanceAt != null && (
        <span className="text-xs text-warning">
          No new heartbeat since <RelativeTime at={new Date(trend.lastAdvanceAt).toISOString()} />
        </span>
      )}
      {trend && trend.increments.length > 1 && (
        <Sparkline
          points={trend.increments.map((value) => ({ value }))}
          className="h-4 w-24 text-accent"
        />
      )}
    </div>
  );
}

/** A replica's generation as its start time (or a number), with the raw value in the tooltip. */
function GenerationCell({ epoch }: { epoch: number }) {
  const generation = describeGeneration(epoch);
  return generation.startedAt ? (
    <time
      dateTime={generation.startedAt.toISOString()}
      title={generation.title}
      className="whitespace-nowrap tabular-nums"
    >
      {generation.label}
    </time>
  ) : (
    <span title={generation.title} className="whitespace-nowrap tabular-nums">
      {generation.label}
    </span>
  );
}

/**
 * A drained replica that has been stopped: the server keeps listing it (drained, with no
 * heartbeat and no mesh address) so its drain request is not lost, until it is undrained.
 */
function isStopped(r: Replica): boolean {
  return r.status === "drained" && r.role !== "single-node" && !r.last_heartbeat_at;
}

function Dash() {
  return <span className="text-text-faint">—</span>;
}

/** The status pill, and for a drain an administrator asked for: who, when, how far, the task. */
function ReplicaStatusCell({ replica }: { replica: Replica }) {
  const meta = replicaStatusMeta(replica.status);
  return (
    <div className="flex min-w-28 max-w-44 flex-col items-start gap-1 text-left">
      <Badge status={meta.status}>{meta.label}</Badge>
      {replica.status === "draining" && replica.drain_task_id && (
        <DrainProgress replica={replica} taskId={replica.drain_task_id} />
      )}
      {replica.status === "draining" && !replica.drain_task_id && (
        <span className="text-xs text-text-muted">
          {plural(replica.shard_count ?? 0, "shard", "shards")} left to hand off
        </span>
      )}
      {isStopped(replica) && (
        <span className="text-xs text-text-muted">
          Not running. It stays listed, and drained, until you undrain it.
        </span>
      )}
      {isDrainRequested(replica) && (replica.drain_requested_by || replica.drain_requested_at) && (
        <span className="text-xs text-text-muted">
          Asked
          {replica.drain_requested_by && (
            <>
              {" by "}
              {/* A Matrix ID has no spaces to wrap at; let it break so the column stays narrow. */}
              <span className="font-identifier wrap-anywhere">{replica.drain_requested_by}</span>
            </>
          )}
          {replica.drain_requested_at && (
            <>
              {" "}
              <RelativeTime at={replica.drain_requested_at} />
            </>
          )}
        </span>
      )}
      {isDrainRequested(replica) && replica.drain_task_id && (
        <Link
          to="/tasks/$taskId"
          params={{ taskId: replica.drain_task_id }}
          search={{}}
          className="text-xs text-accent underline underline-offset-2 hover:no-underline"
          aria-label={`Drain task for ${replica.id}`}
        >
          Drain task
        </Link>
      )}
    </div>
  );
}

/** How far a drain has got, from its task (shards handed off of the total). */
function DrainProgress({ replica, taskId }: { replica: Replica; taskId: string }) {
  const task = useTask(taskId);
  const left = replica.shard_count ?? 0;
  const words = task.data ? describeProgress(task.data) : null;
  return (
    <div className="w-36">
      <TaskProgressBar
        fraction={task.data ? progressFraction(task.data) : null}
        label={
          words
            ? `${words} handed off, ${left.toLocaleString()} left`
            : `${plural(left, "shard", "shards")} left to hand off`
        }
        compact
      />
    </div>
  );
}

function ReplicaActions({
  replica,
  replicas,
  singleNode,
  onAction,
}: {
  replica: Replica;
  replicas: Replica[];
  singleNode: boolean;
  onAction: (pending: PendingReplicaAction) => void;
}) {
  const canWrite = hasScope("admin:write");
  const reasonId = `drain-reason-${replica.id}`;

  if (isDrainRequested(replica)) {
    return (
      <Button
        size="sm"
        variant="secondary"
        disabled={!canWrite}
        title={canWrite ? undefined : "Needs admin:write"}
        aria-label={`Undrain ${replica.id}`}
        onClick={() => onAction({ action: "undrain", replica })}
      >
        Undrain
      </Button>
    );
  }

  const blocked = !canWrite
    ? "Needs the admin:write scope."
    : drainBlockedReason(replica, replicas, singleNode);
  return (
    <div className="flex flex-col items-end gap-1">
      <Button
        size="sm"
        variant="secondary"
        disabled={Boolean(blocked)}
        aria-label={`Drain ${replica.id}`}
        aria-describedby={
          blocked ? (singleNode ? `${reasonId} single-node-note` : reasonId) : undefined
        }
        onClick={() => onAction({ action: "drain", replica })}
      >
        Drain
      </Button>
      {blocked && (
        <span id={reasonId} className="max-w-32 text-right text-xs text-text-muted">
          {blocked}
        </span>
      )}
    </div>
  );
}

const KIND_OPTIONS = [
  { value: "all", label: "Every kind" },
  ...SHARD_KINDS.map((k) => ({ value: k, label: SHARD_KIND_LABELS[k].plural })),
];

function ShardsSection({
  search,
  onSearch,
  replicas,
  allShards,
  moving,
}: {
  search: ClusterSearch;
  onSearch: (patch: Partial<ClusterSearch>) => void;
  replicas: Replica[];
  allShards: ReturnType<typeof useAllShards>;
  moving: boolean;
}) {
  const view: ShardView = search.view ?? "map";
  const shown = (allShards.data ?? []).filter((s) => !search.kind || s.kind === search.kind);

  return (
    <section aria-labelledby="shards-heading" className="space-y-3">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h2 id="shards-heading" className="text-lg text-text">
            Shards
          </h2>
          <p className="text-sm text-text-muted">
            Every room, user, federation destination and appservice hashes onto one shard, and each
            shard has at most one owner. A shard&apos;s epoch goes up each time it changes owner:
            one that keeps rising means its ownership is moving back and forth.
          </p>
        </div>
        <div className="flex flex-wrap items-end gap-3">
          <div className="w-56">
            <Field label="Kind">
              {(props) => (
                <Select
                  {...props}
                  value={search.kind ?? "all"}
                  options={KIND_OPTIONS}
                  onValueChange={(v) =>
                    onSearch({
                      kind: v === "all" ? undefined : (v as ShardKind),
                      cursor: undefined,
                    })
                  }
                />
              )}
            </Field>
          </div>
          <div
            role="group"
            aria-label="Shard view"
            className="inline-flex h-9 overflow-hidden rounded-sm border border-border-strong"
          >
            {(["map", "table"] as const).map((v) => (
              <button
                key={v}
                type="button"
                aria-pressed={view === v}
                onClick={() => onSearch({ view: v === "map" ? undefined : v, cursor: undefined })}
                className={cn(
                  "px-3 text-sm font-medium focus-visible:outline-2 focus-visible:-outline-offset-2 focus-visible:outline-[var(--color-focus)]",
                  view === v
                    ? "bg-accent-muted text-accent"
                    : "bg-surface text-text-muted hover:bg-surface-sunken hover:text-text",
                )}
              >
                {v === "map" ? "Map" : "Table"}
              </button>
            ))}
          </div>
        </div>
      </div>

      {view === "map" ? (
        allShards.isError ? (
          <QueryProblemState
            error={allShards.error}
            resource="shards"
            scope="admin:read"
            onRetry={() => allShards.refetch()}
          />
        ) : allShards.isLoading ? (
          <Skeleton className="h-48 rounded-md" />
        ) : shown.length === 0 ? (
          <div className="rounded-md border border-border bg-surface">
            <NoShards filtered={Boolean(search.kind)} />
          </div>
        ) : (
          <ShardMap shards={shown} replicas={replicas} />
        )
      ) : (
        <ShardTable search={search} onSearch={onSearch} replicas={replicas} moving={moving} />
      )}
    </section>
  );
}

function NoShards({ filtered }: { filtered: boolean }): ReactNode {
  return (
    <EmptyState
      variant={filtered ? "filtered" : "page"}
      icon={<Boxes aria-hidden="true" />}
      title={filtered ? "No shards of this kind" : "No shards"}
      description={
        filtered
          ? "The layout has none of this kind."
          : "The server reported no shards, which it does before its layout is set up."
      }
    />
  );
}

const SHARD_STATE_META: Record<
  string,
  { label: string; status: "success" | "warning" | "neutral" }
> = {
  owned: { label: "Owned", status: "success" },
  released: { label: "Being handed off", status: "warning" },
  unassigned: { label: "Unassigned", status: "neutral" },
};

function ShardTable({
  search,
  onSearch,
  replicas,
  moving,
}: {
  search: ClusterSearch;
  onSearch: (patch: Partial<ClusterSearch>) => void;
  replicas: Replica[];
  moving: boolean;
}) {
  const page = useShardPage(
    { kind: search.kind, cursor: search.cursor, limit: SHARD_PAGE_SIZE },
    { moving },
  );
  const thisReplica = replicas.find((r) => r.this_replica)?.id;

  const columns: Column<Shard>[] = [
    {
      key: "id",
      header: "Shard",
      render: (s) => <span className="font-identifier text-text">{s.id}</span>,
    },
    {
      key: "kind",
      header: "Kind",
      priority: 2,
      render: (s) =>
        s.kind && s.kind in SHARD_KIND_LABELS
          ? SHARD_KIND_LABELS[s.kind as ShardKind].plural
          : (s.kind ?? "—"),
    },
    {
      key: "owner",
      header: "Owner",
      render: (s) =>
        s.owner ? (
          <span className="font-identifier">
            {s.owner}
            {s.owner === thisReplica && (
              <span className="font-sans text-text-muted"> (this replica)</span>
            )}
          </span>
        ) : (
          <span className="text-text-muted">No owner yet</span>
        ),
      renderCompact: (s) => s.owner ?? "No owner yet",
    },
    {
      key: "state",
      header: "State",
      render: (s) => {
        const meta = SHARD_STATE_META[s.state ?? ""] ?? {
          label: s.state ?? "Unknown",
          status: "neutral" as const,
        };
        return (
          <Badge status={meta.status} hideIcon={meta.status === "success"}>
            {meta.label}
          </Badge>
        );
      },
      renderCompact: (s) => SHARD_STATE_META[s.state ?? ""]?.label ?? s.state ?? "—",
    },
    {
      key: "epoch",
      header: "Epoch",
      priority: 2,
      align: "end",
      render: (s) => (s.epoch != null ? s.epoch.toLocaleString() : "—"),
    },
  ];

  if (page.isError) {
    return (
      <QueryProblemState
        error={page.error}
        resource="shards"
        scope="admin:read"
        onRetry={() => page.refetch()}
      />
    );
  }

  return (
    <DataTable
      caption="Shards"
      density="compact"
      columns={columns}
      rows={page.data?.items ?? []}
      getRowId={(s) => s.id ?? ""}
      loading={page.isLoading}
      empty={<NoShards filtered={Boolean(search.kind)} />}
      pagination={{
        hasPrevious: Boolean(search.cursor),
        hasNext: Boolean(page.data?.next_cursor),
        onPrevious: () => onSearch({ cursor: page.data?.prev_cursor ?? undefined }),
        onNext: () => onSearch({ cursor: page.data?.next_cursor ?? undefined }),
      }}
    />
  );
}
