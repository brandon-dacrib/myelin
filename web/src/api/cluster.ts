/**
 * The cluster's replicas and shards (`cluster.replicas.*` and `cluster.shards.list` in
 * `crates/hs-admin/openapi/openapi.yaml`). `useClusterStatus` (`GET /cluster`, the summary the
 * Overview and the top bar also read) lives in `./dashboard`.
 *
 * A drain is asynchronous: `POST /cluster/replicas/{id}/drain` answers the replica as
 * `draining` with a `drain_task_id`, and the replica becomes `drained` once it owns no shards.
 * The replica list polls quickly while any replica is draining, so the page follows the drain
 * without the operator reloading. A server not running as a cluster refuses every drain with a
 * 409 (nothing could take the shards); the page shows that refusal's own `detail`.
 */
import { useEffect, useRef } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import type { components, operations } from "./schema";
import { hasScope } from "@/lib/auth";

export type Replica = components["schemas"]["Replica"];
export type Shard = components["schemas"]["Shard"];
export type ShardKind = NonNullable<
  NonNullable<operations["cluster.shards.list"]["parameters"]["query"]>["kind"]
>;

/** The largest page the server hands out (`crates/hs-admin/src/model.rs` clamps to 500). */
const PAGE_LIMIT = 500;
/** How many pages {@link fetchAll} follows before it stops: 10,000 shards is far past any layout. */
const MAX_PAGES = 20;

/** Polling while something moves (a drain), and at rest. */
const FAST_POLL_MS = 1_500;
const SLOW_POLL_MS = 15_000;

/** Follows `next_cursor` until the last page, for lists the page needs whole. */
async function fetchAll<T>(
  page: (cursor: string | undefined) => Promise<{ items: T[]; next_cursor: string | null }>,
): Promise<T[]> {
  const all: T[] = [];
  let cursor: string | undefined;
  for (let i = 0; i < MAX_PAGES; i += 1) {
    const result = await page(cursor);
    all.push(...result.items);
    if (!result.next_cursor) break;
    cursor = result.next_cursor;
  }
  return all;
}

/** One reading of a replica's heartbeat sequence, as this browser saw it. */
export interface HeartbeatSample {
  /** When the reading was taken (`Date.now()`). */
  at: number;
  seq: number;
}

/** What the readings of one replica's heartbeat sequence say. */
export interface HeartbeatTrend {
  samples: HeartbeatSample[];
  /** Heartbeats between consecutive readings, oldest first; one fewer than the samples. */
  increments: number[];
  /**
   * Whether the sequence moved on since the reading before: the liveness the other replicas
   * watch for. `null` with fewer than two readings.
   */
  advancing: boolean | null;
  /** The reading that last showed a new number, or the first reading when none did. */
  lastAdvanceAt: number | null;
}

/** How many readings are kept per replica: forty polls, ten minutes at the slow interval. */
const HEARTBEAT_HISTORY = 40;

/**
 * The heartbeat sequence of each replica at each poll, kept for the session. The server keeps
 * no history of the sequence (`Replica.heartbeat_seq` is the latest number), so the page
 * remembers what it read and can say whether a replica's heartbeats are still arriving:
 * one whose number has not moved since the last poll (15 seconds, against a heartbeat every
 * two by default) has stopped reaching the store.
 */
const heartbeatHistory = new Map<string, HeartbeatSample[]>();

/** Forgets every reading (tests, and a sign-out). */
export function resetHeartbeatHistory(): void {
  heartbeatHistory.clear();
}

/** Remembers the sequence each replica reports; a replica without one (a single node) is skipped. */
export function recordHeartbeats(replicas: Replica[], at: number = Date.now()): void {
  for (const replica of replicas) {
    if (!replica.id || replica.heartbeat_seq == null) continue;
    const samples = heartbeatHistory.get(replica.id) ?? [];
    if (samples.at(-1)?.at === at) continue;
    samples.push({ at, seq: replica.heartbeat_seq });
    if (samples.length > HEARTBEAT_HISTORY) samples.splice(0, samples.length - HEARTBEAT_HISTORY);
    heartbeatHistory.set(replica.id, samples);
  }
}

/** The readings of `id`'s heartbeat sequence so far, and what they say. */
export function heartbeatTrend(id: string): HeartbeatTrend {
  const samples = heartbeatHistory.get(id) ?? [];
  const increments = samples.slice(1).map((s, i) => Math.max(s.seq - samples[i].seq, 0));
  let lastAdvanceAt: number | null = samples[0]?.at ?? null;
  samples.forEach((s, i) => {
    if (i > 0 && s.seq > samples[i - 1].seq) lastAdvanceAt = s.at;
  });
  return {
    samples,
    increments,
    advancing: increments.length === 0 ? null : increments[increments.length - 1] > 0,
    lastAdvanceAt,
  };
}

/** Whether a replica is in the middle of something the page should follow closely. */
export function replicaIsMoving(replica: Pick<Replica, "status">): boolean {
  return replica.status === "draining" || replica.status === "joining";
}

/** Every replica, polled every 1.5s while one is draining or joining and every 15s otherwise. */
export function useReplicas() {
  return useQuery({
    queryKey: ["cluster-replicas"],
    enabled: hasScope("admin:read"),
    queryFn: async () => {
      const replicas = await fetchAll(async (cursor) =>
        unwrap(
          await api.GET("/cluster/replicas", {
            params: { query: { limit: PAGE_LIMIT, cursor } },
          }),
        ),
      );
      recordHeartbeats(replicas);
      return replicas;
    },
    refetchInterval: (query) =>
      query.state.data?.some(replicaIsMoving) ? FAST_POLL_MS : SLOW_POLL_MS,
  });
}

/** Every shard of the layout, for the shard map and the summary. Polls fast while `moving`. */
export function useAllShards(options: { moving: boolean }) {
  return useQuery({
    queryKey: ["cluster-shards", "all"],
    enabled: hasScope("admin:read"),
    queryFn: () =>
      fetchAll(async (cursor) =>
        unwrap(
          await api.GET("/cluster/shards", {
            params: { query: { limit: PAGE_LIMIT, cursor } },
          }),
        ),
      ),
    refetchInterval: options.moving ? FAST_POLL_MS : SLOW_POLL_MS,
  });
}

/**
 * Reads the shards again the moment the replicas stop moving. While a drain runs the shards poll
 * fast; when the replicas say it is over, the shard polls drop to the slow interval, and the last
 * read may be from just before the end (a shard between owners, or one still on the drained
 * replica). Without this the summary would say a shard has no owner for up to 15 seconds after
 * the drain finished.
 */
export function useRefreshShardsWhenSettled(moving: boolean) {
  const qc = useQueryClient();
  const wasMoving = useRef(moving);
  useEffect(() => {
    if (wasMoving.current && !moving) {
      void qc.invalidateQueries({ queryKey: ["cluster-shards"] });
    }
    wasMoving.current = moving;
  }, [moving, qc]);
}

export interface ShardPageFilters {
  kind?: ShardKind;
  cursor?: string;
  limit?: number;
}

/** One page of shards, for the table view (the server does the filtering and the paging). */
export function useShardPage(filters: ShardPageFilters, options: { moving: boolean }) {
  return useQuery({
    queryKey: ["cluster-shards", "page", filters],
    enabled: hasScope("admin:read"),
    queryFn: async () => unwrap(await api.GET("/cluster/shards", { params: { query: filters } })),
    refetchInterval: options.moving ? FAST_POLL_MS : SLOW_POLL_MS,
  });
}

function useReplicaAction(action: "drain" | "undrain") {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      const params = { path: { id }, header: { "Idempotency-Key": newIdempotencyKey() } };
      const result =
        action === "drain"
          ? await api.POST("/cluster/replicas/{id}/drain", { params })
          : await api.POST("/cluster/replicas/{id}/undrain", { params });
      return unwrap(result);
    },
    onSuccess: (replica) => {
      // The answer is the replica as it is now; put it in the list at once so the row changes
      // before the next poll, then read everything it moved.
      qc.setQueryData<Replica[]>(["cluster-replicas"], (list) =>
        list?.map((r) => (r.id === replica.id ? replica : r)),
      );
      qc.invalidateQueries({ queryKey: ["cluster-replicas"] });
      qc.invalidateQueries({ queryKey: ["cluster-shards"] });
      qc.invalidateQueries({ queryKey: ["cluster-status"] });
      qc.invalidateQueries({ queryKey: ["tasks"] });
      if (replica.drain_task_id)
        qc.invalidateQueries({ queryKey: ["task", replica.drain_task_id] });
    },
  });
}

/** `POST /cluster/replicas/{id}/drain` (admin:write). */
export function useDrainReplica() {
  return useReplicaAction("drain");
}

/** `POST /cluster/replicas/{id}/undrain` (admin:write). */
export function useUndrainReplica() {
  return useReplicaAction("undrain");
}
