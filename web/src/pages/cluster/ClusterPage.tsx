import { useState, type ReactNode } from "react";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { Boxes, Server } from "lucide-react";
import {
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
      />

      <section aria-labelledby="replicas-heading" className="space-y-3">
        <h2 id="replicas-heading" className="text-lg text-text">
          Replicas
        </h2>
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
}: {
  singleNode: boolean;
  statusKnown: boolean;
  replicas: Replica[] | undefined;
  replicaCount: number | undefined;
  shards: Shard[] | undefined;
  shardCount: number | undefined;
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

  return (
    <dl className="grid gap-3 sm:grid-cols-3">
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
    </dl>
  );
}

function SummaryTile({ label, value, note }: { label: string; value: string; note?: string }) {
  return (
    <div className="rounded-md border border-border bg-surface p-4">
      <dt className="text-xs font-medium uppercase tracking-wide text-text-muted">{label}</dt>
      <dd className="mt-1 text-2xl text-text tabular-nums">{value}</dd>
      {note && <dd className="mt-0.5 text-xs text-text-muted">{note}</dd>}
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
      render: (r) =>
        r.last_heartbeat_at ? (
          <RelativeTime at={r.last_heartbeat_at} />
        ) : r.role === "single-node" ? (
          <span className="text-text-faint" title="A single node sends no heartbeats">
            —
          </span>
        ) : (
          <span className="whitespace-nowrap text-text-muted">Not running</span>
        ),
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
      render: (r) =>
        r.mesh_addr ? (
          <span className="whitespace-nowrap font-identifier">{r.mesh_addr}</span>
        ) : (
          <Dash />
        ),
    },
    {
      key: "epoch",
      header: "Epoch",
      priority: 3,
      align: "end",
      // A stopped replica has no generation (the server answers 0).
      render: (r) => (isStopped(r) || !r.epoch ? <Dash /> : r.epoch.toLocaleString()),
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
    <div className="flex min-w-40 max-w-52 flex-col items-start gap-1 text-left">
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
              <span className="font-identifier">{replica.drain_requested_by}</span>
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
    <div className="w-40">
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
        <span id={reasonId} className="max-w-48 text-right text-xs text-text-muted">
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
            shard has at most one owner.
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
          <span className="text-text-muted">unowned</span>
        ),
      renderCompact: (s) => s.owner ?? "unowned",
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
