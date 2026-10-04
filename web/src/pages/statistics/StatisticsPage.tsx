import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { DoorOpen, Image } from "lucide-react";
import { useClusterStatus, useStatisticsOverview } from "@/api/dashboard";
import {
  RANGES,
  useRoomStatistics,
  useUserMediaStatistics,
  type RangeId,
  type RoomStatistic,
  type UserMediaStatistic,
} from "@/api/statistics";
import { QueryProblemState } from "@/components/QueryProblemState";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { Field } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { Skeleton } from "@/components/ui/skeleton/Skeleton";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { hasScope } from "@/lib/auth";
import { formatBytes, formatCount } from "@/lib/format";
import { MetricCard } from "./MetricCard";
import { fromSortState, toSortState, type StatisticsSearch } from "./statistics-search";

const count = (value: number) => formatCount(value);

/**
 * `/statistics`: how the server is used over time, which rooms are largest, and whose media
 * takes the space. Every number comes from `statistics.*` (`crates/hs-admin/src/statistics.rs`).
 */
export function StatisticsPage() {
  const search = useSearch({ from: "/statistics" });
  const navigate = useNavigate({ from: "/statistics" });
  const range: RangeId = search.range ?? "7d";
  const update = (patch: Partial<StatisticsSearch>) =>
    navigate({ search: { ...search, ...patch }, replace: true });

  if (!hasScope("admin:read"))
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Statistics</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );

  return (
    <div className="mx-auto max-w-[90rem] space-y-8 p-6">
      <div className="flex flex-wrap items-end justify-between gap-4">
        <div>
          <h1 className="text-xl text-text">Statistics</h1>
          <p className="mt-1 text-sm text-text-muted">
            How this server is used over time, its largest rooms, and whose uploads take the space.
          </p>
        </div>
        <div className="w-56">
          <Field label="Range">
            {(props) => (
              <Select
                {...props}
                value={range}
                onValueChange={(value) =>
                  update({ range: value === "7d" ? undefined : (value as RangeId) })
                }
                options={Object.entries(RANGES).map(([value, r]) => ({ value, label: r.label }))}
              />
            )}
          </Field>
        </div>
      </div>

      <NowTiles />

      <section aria-labelledby="activity-heading" className="space-y-3">
        <h2 id="activity-heading" className="text-md font-medium text-text">
          Activity
        </h2>
        <div className="grid grid-cols-1 gap-4 xl:grid-cols-2">
          <MetricCard
            metric="daily_active_users"
            title="Daily active users"
            description="People who used the server in the 24 hours before each sample."
            range={range}
            formatValue={count}
          />
          <MetricCard
            metric="users.registered"
            title="New accounts"
            description="Accounts created in each period."
            range={range}
            formatValue={count}
          />
          <MetricCard
            metric="media.uploaded_bytes"
            title="Media uploaded"
            description="The size of what local users uploaded in each period."
            range={range}
            formatValue={formatBytes}
          />
          <MetricCard
            metric="reports.received"
            title="Reports received"
            description="Messages, rooms and people reported in each period."
            range={range}
            formatValue={count}
          />
          <MetricCard
            metric="users_count"
            title="Accounts"
            description="Local accounts on the server."
            range={range}
            formatValue={count}
          />
          <MetricCard
            metric="media_bytes"
            title="Media stored"
            description="The size of all media this server keeps."
            range={range}
            formatValue={formatBytes}
          />
        </div>
      </section>

      <div className="grid grid-cols-1 gap-8 xl:grid-cols-2">
        <LargestRooms search={search} update={update} />
        <MediaByUser search={search} update={update} />
      </div>

      <ClusterNow />
    </div>
  );
}

function NowTiles() {
  const stats = useStatisticsOverview();
  const tiles: { label: string; value: string }[] = [
    { label: "Accounts", value: formatCount(stats.data?.users_count) },
    { label: "Rooms", value: formatCount(stats.data?.rooms_count) },
    { label: "Daily active", value: formatCount(stats.data?.daily_active_users) },
    { label: "Monthly active", value: formatCount(stats.data?.monthly_active_users) },
    { label: "Media files", value: formatCount(stats.data?.media_count) },
    { label: "Media stored", value: formatBytes(stats.data?.media_bytes) },
  ];
  return (
    <section aria-labelledby="now-heading" className="space-y-3">
      <h2 id="now-heading" className="text-md font-medium text-text">
        Now
      </h2>
      {stats.isError ? (
        <QueryProblemState
          error={stats.error}
          resource="the server's counts"
          scope="admin:read"
          compact
          onRetry={() => stats.refetch()}
        />
      ) : (
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 xl:grid-cols-6">
          {tiles.map((tile) =>
            stats.isLoading ? (
              <Skeleton key={tile.label} className="h-20 rounded-md" />
            ) : (
              <div key={tile.label} className="rounded-md border border-border bg-surface p-4">
                <p className="text-xs text-text-muted">{tile.label}</p>
                <p className="mt-1 text-2xl text-text tabular-nums">{tile.value}</p>
              </div>
            ),
          )}
        </div>
      )}
    </section>
  );
}

/**
 * The cluster's numbers now, from `GET /cluster`: how many replicas, the answering replica's
 * heartbeat sequence and how many drains released at once. The server keeps no history of these
 * (they are not among `statistics.timeseries`' metrics), so there is no chart; the Cluster page
 * follows each replica's heartbeat while it is open.
 */
function ClusterNow() {
  const cluster = useClusterStatus();
  const singleNode = cluster.data?.mode === "single-node";
  const tiles: { label: string; value: string; description: string }[] = [
    {
      label: "Replicas",
      value: formatCount(cluster.data?.replica_count),
      description: "Registered and heartbeating, whatever their status.",
    },
    {
      label: "Heartbeat sequence",
      value: formatCount(cluster.data?.heartbeat_seq),
      description:
        "The answering replica's last heartbeat to reach the store; one more per heartbeat.",
    },
    {
      label: "Drains released at once",
      value: formatCount(cluster.data?.drain_released_at_once_count),
      description:
        "Drains that let go of every shard at once rather than one lease at a time, since the answering replica started.",
    },
  ];
  return (
    <section aria-labelledby="cluster-now-heading" className="space-y-3">
      <h2 id="cluster-now-heading" className="text-md font-medium text-text">
        Cluster
      </h2>
      {cluster.isError ? (
        <QueryProblemState
          error={cluster.error}
          resource="the cluster's numbers"
          scope="admin:read"
          compact
          onRetry={() => cluster.refetch()}
        />
      ) : cluster.isLoading ? (
        <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
          {tiles.map((tile) => (
            <Skeleton key={tile.label} className="h-24 rounded-md" />
          ))}
        </div>
      ) : singleNode ? (
        <p className="text-sm text-text-muted">
          This server runs as a single node: one replica with no heartbeats to count and nothing to
          drain. These numbers appear when it runs as a cluster.
        </p>
      ) : (
        <>
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
            {tiles.map((tile) => (
              <div key={tile.label} className="rounded-md border border-border bg-surface p-4">
                <p className="text-xs text-text-muted">{tile.label}</p>
                <p className="mt-1 text-2xl text-text tabular-nums">{tile.value}</p>
                <p className="mt-1 text-xs text-text-faint">{tile.description}</p>
              </div>
            ))}
          </div>
          <p className="text-xs text-text-faint">
            As the replica answering this page sees them now. The server keeps no history of these
            yet, so there is no chart; the{" "}
            <Link to="/cluster" search={{}} className="text-accent hover:underline">
              Cluster page
            </Link>{" "}
            follows every replica&apos;s heartbeat while it is open.
          </p>
        </>
      )}
    </section>
  );
}

interface TableProps {
  search: StatisticsSearch;
  update: (patch: Partial<StatisticsSearch>) => void;
}

function LargestRooms({ search, update }: TableProps) {
  const sort = search.rooms_sort ?? "-joined_members_count";
  const query = useRoomStatistics({ sort, cursor: search.rooms_cursor, limit: 10 });
  const columns: Column<RoomStatistic>[] = [
    {
      key: "room",
      header: "Room",
      interactive: true,
      render: (room) =>
        room.room_id ? (
          <Link
            to="/rooms/$roomId"
            params={{ roomId: room.room_id }}
            className="break-all text-text hover:text-accent hover:underline"
          >
            {room.name ?? <span className="font-identifier">{room.room_id}</span>}
          </Link>
        ) : (
          (room.name ?? "—")
        ),
    },
    {
      key: "joined_members_count",
      header: "Members",
      sortable: true,
      align: "end",
      render: (room) => formatCount(room.joined_members_count),
    },
    {
      key: "state_events_count",
      header: "State events",
      sortable: true,
      align: "end",
      render: (room) => formatCount(room.state_events_count),
    },
  ];
  return (
    <section aria-labelledby="largest-rooms-heading" className="space-y-3">
      <div>
        <h2 id="largest-rooms-heading" className="text-md font-medium text-text">
          Largest rooms
        </h2>
        <p className="text-xs text-text-muted">
          By joined members, or by state events: a room with a lot of state is slower to join.
        </p>
      </div>
      {query.isError ? (
        <QueryProblemState
          error={query.error}
          resource="room statistics"
          scope="admin:read"
          onRetry={() => query.refetch()}
        />
      ) : (
        <DataTable
          caption="Largest rooms"
          columns={columns}
          rows={query.data?.items ?? []}
          getRowId={(room) => room.room_id ?? room.name ?? ""}
          loading={query.isLoading}
          density="compact"
          sort={toSortState(sort)}
          onSortChange={(next) =>
            update({ rooms_sort: fromSortState(next), rooms_cursor: undefined })
          }
          empty={
            <EmptyState
              icon={<DoorOpen aria-hidden="true" />}
              title="No rooms yet"
              description="Rooms appear here once people create or join them."
            />
          }
          pagination={{
            hasPrevious: Boolean(search.rooms_cursor),
            hasNext: Boolean(query.data?.next_cursor),
            onPrevious: () => update({ rooms_cursor: query.data?.prev_cursor ?? undefined }),
            onNext: () => update({ rooms_cursor: query.data?.next_cursor ?? undefined }),
          }}
        />
      )}
    </section>
  );
}

function MediaByUser({ search, update }: TableProps) {
  const sort = search.media_sort ?? "-media_bytes";
  const query = useUserMediaStatistics({ sort, cursor: search.media_cursor, limit: 10 });
  const columns: Column<UserMediaStatistic>[] = [
    {
      key: "user",
      header: "Person",
      interactive: true,
      render: (row) =>
        row.user_id ? (
          <Link
            to="/users/$userId"
            params={{ userId: row.user_id }}
            className="break-all font-identifier text-text hover:text-accent hover:underline"
          >
            {row.user_id}
          </Link>
        ) : (
          "—"
        ),
    },
    {
      key: "media_count",
      header: "Files",
      sortable: true,
      align: "end",
      render: (row) => formatCount(row.media_count),
    },
    {
      key: "media_bytes",
      header: "Size",
      sortable: true,
      align: "end",
      render: (row) => formatBytes(row.media_bytes),
    },
  ];
  return (
    <section aria-labelledby="media-by-user-heading" className="space-y-3">
      <div>
        <h2 id="media-by-user-heading" className="text-md font-medium text-text">
          Media by person
        </h2>
        <p className="text-xs text-text-muted">What each local account has uploaded.</p>
      </div>
      {query.isError ? (
        <QueryProblemState
          error={query.error}
          resource="media usage"
          scope="admin:read"
          onRetry={() => query.refetch()}
        />
      ) : (
        <DataTable
          caption="Media by person"
          columns={columns}
          rows={query.data?.items ?? []}
          getRowId={(row) => row.user_id ?? ""}
          loading={query.isLoading}
          density="compact"
          sort={toSortState(sort)}
          onSortChange={(next) =>
            update({ media_sort: fromSortState(next), media_cursor: undefined })
          }
          empty={
            <EmptyState
              icon={<Image aria-hidden="true" />}
              title="No uploads yet"
              description="Once people share pictures and files, the largest uploaders appear here."
            />
          }
          pagination={{
            hasPrevious: Boolean(search.media_cursor),
            hasNext: Boolean(query.data?.next_cursor),
            onPrevious: () => update({ media_cursor: query.data?.prev_cursor ?? undefined }),
            onNext: () => update({ media_cursor: query.data?.next_cursor ?? undefined }),
          }}
        />
      )}
    </section>
  );
}
