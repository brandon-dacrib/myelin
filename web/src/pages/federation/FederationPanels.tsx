import { useEffect, useRef, useState } from "react";
import { Link } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { DoorOpen, KeyRound, RefreshCw } from "lucide-react";
import {
  useDestinationRooms,
  useOwnSigningKeys,
  useRefreshRemoteKeys,
  useRemoteKeys,
  type DestinationRoom,
  type SigningKey,
} from "@/api/federation";
import { ApiProblemError } from "@/api/problem";
import { taskIsActive, useTask } from "@/api/tasks";
import { CopyableId } from "@/components/CopyableId";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import { formatCount } from "@/lib/format";
import { TaskProgressBar } from "../tasks/TaskProgress";

/** A public key, shortened for a table cell and copyable in full. */
function PublicKey({ value }: { value: string }) {
  return <CopyableId value={value} label="public key" />;
}

const keyColumns: Column<SigningKey>[] = [
  {
    key: "key_id",
    header: "Key",
    priority: 1,
    render: (k) => <span className="font-identifier">{k.key_id}</span>,
  },
  {
    key: "public_key",
    header: "Public key",
    priority: 2,
    render: (k) => <PublicKey value={k.public_key} />,
  },
  {
    key: "valid_until_at",
    header: "Valid until",
    priority: 2,
    render: (k) =>
      k.valid_until_at ? (
        <RelativeTime at={k.valid_until_at} />
      ) : (
        <span className="text-text-muted">Until rotated</span>
      ),
  },
  {
    key: "old",
    header: "Kind",
    priority: 3,
    render: (k) =>
      k.old ? <Badge status="muted">Old</Badge> : <Badge status="success">Current</Badge>,
    renderCompact: (k) => (k.old ? "Old" : "Current"),
  },
];

/** The Federation page's panel of this server's own signing keys. */
export function OwnKeysPanel() {
  const { data, isLoading, isError, error, refetch } = useOwnSigningKeys();
  return (
    <section aria-labelledby="own-keys" className="mt-8">
      <h2 id="own-keys" className="text-lg text-text">
        This server&apos;s signing keys
      </h2>
      <p className="mt-1 text-sm text-text-muted">
        Other servers check this server&apos;s events and requests against these keys, published at{" "}
        <span className="font-identifier">/_matrix/key/v2/server</span>.
      </p>
      <div className="mt-3">
        {isError ? (
          <QueryProblemState error={error} resource="signing keys" onRetry={() => refetch()} />
        ) : (
          <DataTable
            caption="This server's signing keys"
            columns={keyColumns}
            rows={data ?? []}
            getRowId={(k) => k.key_id}
            loading={isLoading}
            density="compact"
          />
        )}
      </div>
    </section>
  );
}

/**
 * A destination's signing keys as this server's cache holds them, and a way to fetch them
 * again: the refetch is a task, followed here until it ends.
 */
export function RemoteKeysPanel({ serverName }: { serverName: string }) {
  const qc = useQueryClient();
  const { data, isLoading, isError, error, refetch } = useRemoteKeys(serverName);
  const refresh = useRefreshRemoteKeys();
  const [taskId, setTaskId] = useState<string>();
  const { data: task } = useTask(taskId);
  const announced = useRef<string | undefined>(undefined);
  const canWrite = hasScope("admin:write");

  useEffect(() => {
    if (!task || taskIsActive(task) || announced.current === task.id) return;
    announced.current = task.id;
    setTaskId(undefined);
    void qc.invalidateQueries({ queryKey: ["federation-remote-keys", serverName] });
    if (task.status === "succeeded") {
      toast({ title: `Fetched ${serverName}'s keys again` });
    } else if (task.status === "failed") {
      toast({
        title: `Couldn't fetch ${serverName}'s keys`,
        description: task.error?.detail ?? undefined,
        variant: "danger",
      });
    }
  }, [task, qc, serverName]);

  const running = task && taskIsActive(task);
  return (
    <section aria-labelledby="remote-keys" className="mt-8">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h2 id="remote-keys" className="text-lg text-text">
            Signing keys
          </h2>
          <p className="mt-1 text-sm text-text-muted">
            What this server checks {serverName}&apos;s signatures against.
            {data?.cached_at && (
              <>
                {" "}
                Fetched <RelativeTime at={data.cached_at} />.
              </>
            )}
          </p>
        </div>
        <Button
          variant="secondary"
          disabled={!canWrite || refresh.isPending || Boolean(running)}
          title={!canWrite ? "Needs admin:write" : undefined}
          onClick={() =>
            refresh.mutate(serverName, {
              onSuccess: (started) => setTaskId(started.id),
              onError: (e) =>
                toast({
                  title: "Couldn't start fetching keys",
                  description: e instanceof ApiProblemError ? e.problem.detail : e.message,
                  variant: "danger",
                }),
            })
          }
        >
          <RefreshCw size={16} aria-hidden="true" />
          Fetch keys again
        </Button>
      </div>
      {running && (
        <div className="mt-3 max-w-md" role="status">
          <TaskProgressBar fraction={null} label={`Fetching ${serverName}'s keys`} />
        </div>
      )}
      <div className="mt-3">
        {isError ? (
          <QueryProblemState error={error} resource="cached keys" onRetry={() => refetch()} />
        ) : data === null ? (
          <EmptyState
            icon={<KeyRound aria-hidden="true" />}
            title="No keys cached"
            description={`This server has not needed to check one of ${serverName}'s signatures since it started. Fetch them to see what it publishes.`}
          />
        ) : (
          <DataTable
            caption={`${serverName}'s signing keys`}
            columns={keyColumns}
            rows={data?.keys ?? []}
            getRowId={(k) => `${k.key_id}${k.old ? ":old" : ""}`}
            loading={isLoading}
            density="compact"
          />
        )}
      </div>
    </section>
  );
}

const roomColumns: Column<DestinationRoom>[] = [
  {
    key: "room",
    header: "Room",
    priority: 1,
    interactive: true,
    render: (r) => (
      <Link
        to="/rooms/$roomId"
        params={{ roomId: r.room_id }}
        className="font-medium text-text hover:text-accent hover:underline"
      >
        {r.name ?? r.canonical_alias ?? <span className="font-identifier">{r.room_id}</span>}
      </Link>
    ),
  },
  {
    key: "destination_members_count",
    header: "Their members",
    priority: 1,
    align: "end",
    render: (r) => formatCount(r.destination_members_count),
  },
  {
    key: "joined_members_count",
    header: "All members",
    priority: 2,
    align: "end",
    render: (r) => formatCount(r.joined_members_count),
  },
];

/** The rooms this server shares with a destination. */
export function DestinationRoomsPanel({ serverName }: { serverName: string }) {
  const { data, isLoading, isError, error, refetch } = useDestinationRooms(serverName);
  return (
    <section aria-labelledby="shared-rooms" className="mt-8">
      <h2 id="shared-rooms" className="text-lg text-text">
        Shared rooms
      </h2>
      <p className="mt-1 text-sm text-text-muted">
        Rooms here with {serverName}&apos;s users in them: what this server sends there.
        {data?.total != null && ` ${formatCount(data.total)} in all.`}
      </p>
      <div className="mt-3">
        {isError ? (
          <QueryProblemState error={error} resource="shared rooms" onRetry={() => refetch()} />
        ) : (
          <DataTable
            caption={`Rooms shared with ${serverName}`}
            columns={roomColumns}
            rows={data?.items ?? []}
            getRowId={(r) => r.room_id}
            loading={isLoading}
            density="compact"
            empty={
              <EmptyState
                icon={<DoorOpen aria-hidden="true" />}
                title="No shared rooms"
                description={`None of ${serverName}'s users is in a room here now.`}
              />
            }
          />
        )}
      </div>
    </section>
  );
}
