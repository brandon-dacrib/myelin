import type { components } from "@/api/schema";
import { cancelTask, getTask, putTask } from "./tasks";

type Replica = components["schemas"]["Replica"];
type Shard = components["schemas"]["Shard"];
type ClusterStatus = components["schemas"]["ClusterStatus"];
type Kind = "room" | "user" | "federation" | "appservice" | "global";

/**
 * The mock's cluster: three replicas sharing a small layout (the real default is 256 room and
 * 256 user shards; fewer here keeps the map readable at a glance). Draining is what the server
 * does, over a few seconds: the replica answers `draining` with a task, its shards go to the
 * other active replicas one by one (each "released" for a moment on the way), and it is
 * `drained` once it owns none, with the task succeeded. Undrain gives it back what it gave away
 * and cancels the task if it was still running. Everything is mutable module state, put back by
 * {@link resetCluster} after every Vitest test.
 */

const KINDS: readonly Kind[] = ["room", "user", "federation", "appservice", "global"];
const LAYOUT: Record<Kind, number> = {
  room: 64,
  user: 32,
  federation: 16,
  appservice: 8,
  global: 1,
};
const OPERATOR = "@admin:example.org";

/** How long a drain takes in the mock; Vitest shortens it (see {@link setDrainDuration}). */
const DEFAULT_DRAIN_MS = 5_000;
let drainMs = DEFAULT_DRAIN_MS;

interface Handoff {
  shard: string;
  to: string;
  done: boolean;
}

interface Drain {
  replica: string;
  startedAt: number;
  handoffs: Handoff[];
  taskId: string;
}

interface State {
  replicas: Replica[];
  shards: Shard[];
  /** Drains still moving shards, by replica. */
  drains: Map<string, Drain>;
  /** What each drained replica handed off, so undrain can give it back. */
  handedOff: Map<string, Handoff[]>;
  taskCounter: number;
}

function seed(): State {
  const replicas: Replica[] = [
    {
      id: "hs-0",
      role: "replica",
      status: "active",
      shard_count: 0,
      epoch: 4,
      this_replica: true,
      mesh_addr: "10.0.1.10:7600",
      version: "0.1.0-dev",
      zone: "eu-west-1a",
      last_heartbeat_at: null,
      drain_requested_at: null,
      drain_requested_by: null,
      drain_task_id: null,
    },
    {
      id: "hs-1",
      role: "replica",
      status: "active",
      shard_count: 0,
      epoch: 2,
      this_replica: false,
      mesh_addr: "10.0.1.11:7600",
      version: "0.1.0-dev",
      zone: "eu-west-1b",
      last_heartbeat_at: null,
      drain_requested_at: null,
      drain_requested_by: null,
      drain_task_id: null,
    },
    {
      id: "hs-2",
      role: "replica",
      status: "active",
      shard_count: 0,
      epoch: 3,
      this_replica: false,
      mesh_addr: "10.0.1.12:7600",
      version: "0.1.0-dev",
      zone: "eu-west-1c",
      last_heartbeat_at: null,
      drain_requested_at: null,
      drain_requested_by: null,
      drain_task_id: null,
    },
  ];
  const shards: Shard[] = [];
  let n = 0;
  for (const kind of KINDS) {
    for (let index = 0; index < LAYOUT[kind]; index += 1) {
      // A spread that looks like rendezvous hashing rather than stripes.
      const owner = replicas[(index * 7 + n * 3 + (index >> 2)) % replicas.length].id ?? null;
      shards.push({ kind, id: `${kind}/${index}`, owner, state: "owned", epoch: 1 + (n % 4) });
      n += 1;
    }
  }
  const state: State = {
    replicas,
    shards,
    drains: new Map(),
    handedOff: new Map(),
    taskCounter: 0,
  };
  recount(state);
  return state;
}

let state = seed();

/** Puts the cluster back as it was (Vitest runs this after every test). */
export function resetCluster(): void {
  state = seed();
  drainMs = DEFAULT_DRAIN_MS;
}

/** How long the next drains take; `0` finishes a drain on the next read. */
export function setDrainDuration(ms: number): void {
  drainMs = ms;
}

function recount(s: State): void {
  for (const r of s.replicas) r.shard_count = s.shards.filter((sh) => sh.owner === r.id).length;
}

function shardById(id: string): Shard | undefined {
  return state.shards.find((s) => s.id === id);
}

function moveShard(shardId: string, to: string): void {
  const shard = shardById(shardId);
  if (!shard) return;
  shard.owner = to;
  shard.state = "owned";
  shard.epoch = (shard.epoch ?? 0) + 1;
}

/**
 * Moves every drain on by the clock: the handoffs due so far are made, the next one is
 * "released" (between owners), and a drain with nothing left is finished. Called before every
 * read, so what the handlers answer is always current.
 */
export function settleCluster(now = Date.now()): void {
  for (const drain of state.drains.values()) {
    const total = drain.handoffs.length;
    const fraction = drainMs <= 0 ? 1 : Math.min((now - drain.startedAt) / drainMs, 1);
    const due = Math.floor(fraction * total);
    drain.handoffs.forEach((h, i) => {
      if (i < due && !h.done) {
        moveShard(h.shard, h.to);
        h.done = true;
      }
    });
    const next = drain.handoffs[due];
    if (next && !next.done && due > 0) {
      const shard = shardById(next.shard);
      if (shard) {
        shard.owner = null;
        shard.state = "released";
      }
    }
    const replica = state.replicas.find((r) => r.id === drain.replica);
    const task = getTask(drain.taskId);
    if (due >= total) {
      if (replica) replica.status = "drained";
      state.handedOff.set(drain.replica, drain.handoffs);
      state.drains.delete(drain.replica);
      if (task && task.status === "running") {
        putTask({
          ...task,
          status: "succeeded",
          finished_at: new Date(now).toISOString(),
          progress: { current: total, total, unit: "shards" },
          result: { handed_off: total },
        });
      }
    } else if (task && task.status === "running") {
      putTask({
        ...task,
        progress: {
          current: due,
          total,
          unit: "shards",
          message: `Handing ${next?.shard ?? "shards"} to ${next?.to ?? "the others"}`,
        },
      });
    }
  }
  recount(state);
}

/** When the mock started: its replicas have been heartbeating since. */
const BOOT = Date.now();
/** How often a mock replica heartbeats, as the server's default (`cluster.heartbeat_interval`). */
const HEARTBEAT_MS = 2_000;

/**
 * A replica's heartbeat sequence: one more every two seconds since the mock started, on top of
 * a number that stands for the heartbeats before (a replica that has run longer has sent more).
 * One that is unreachable stopped a minute and a half ago and its number stands still.
 */
function heartbeatSeq(replica: Replica, now: number): number {
  const before = 10_000 * (replica.epoch ?? 1);
  const until = replica.status === "unreachable" ? BOOT : now;
  return before + Math.floor((until - BOOT) / HEARTBEAT_MS);
}

/** A replica as the wire has it: a heartbeat a moment ago for a replica that is up. */
function wire(replica: Replica): Replica {
  const now = Date.now();
  const offset = replica.this_replica ? 800 : 1_600 + (replica.epoch ?? 0) * 300;
  return {
    ...replica,
    last_heartbeat_at:
      replica.status === "unreachable"
        ? new Date(now - 90_000).toISOString()
        : new Date(now - offset).toISOString(),
    heartbeat_seq: heartbeatSeq(replica, now),
  };
}

export function listReplicas(): Replica[] {
  settleCluster();
  return state.replicas.map(wire);
}

export function getReplica(id: string): Replica | undefined {
  settleCluster();
  const replica = state.replicas.find((r) => r.id === id);
  return replica && wire(replica);
}

export function listShards(kind: string | null): Shard[] {
  settleCluster();
  return state.shards.filter((s) => !kind || s.kind === kind).map((s) => ({ ...s }));
}

export function clusterSummary(): ClusterStatus {
  settleCluster();
  const me = state.replicas.find((r) => r.this_replica);
  return {
    mode: "cluster",
    epoch: 7,
    replica_count: state.replicas.length,
    shard_count: state.shards.length,
    heartbeat_seq: me ? heartbeatSeq(me, Date.now()) : undefined,
    // The mock hands shards off one at a time; it never releases them at once.
    drain_released_at_once_count: 0,
  };
}

export type ReplicaOutcome =
  | { replica: Replica }
  | { problem: "not-found" | "conflict"; detail: string; reason?: DrainRefusal };

/** The drain 409's machine-readable `reason`, as the server words it. */
export type DrainRefusal = "single_node" | "no_other_active_replica";

/** `POST /cluster/replicas/{id}/drain`, as the server answers it. */
export function drainReplica(id: string, by = OPERATOR, now = Date.now()): ReplicaOutcome {
  settleCluster(now);
  const replica = state.replicas.find((r) => r.id === id);
  if (!replica) return { problem: "not-found", detail: `no replica "${id}" is registered` };
  if (replica.status === "draining" || replica.status === "drained")
    return { replica: wire(replica) };
  const takers = state.replicas.filter((r) => r.id !== id && r.status === "active");
  if (takers.length === 0) {
    return replica.role === "single-node"
      ? {
          problem: "conflict",
          reason: "single_node",
          detail:
            "this server is not running as a cluster: there is no other replica to take its shards",
        }
      : {
          problem: "conflict",
          reason: "no_other_active_replica",
          detail: `no other replica is active to take ${id}'s shards; start or undrain another replica first`,
        };
  }
  const owned = state.shards.filter((s) => s.owner === id);
  const handoffs = owned.map((s, i) => ({
    shard: s.id ?? "",
    to: takers[i % takers.length].id ?? "",
    done: false,
  }));
  state.taskCounter += 1;
  const taskId = `task_drain_${String(state.taskCounter).padStart(4, "0")}`;
  const at = new Date(now).toISOString();
  putTask({
    id: taskId,
    action: "cluster.replicas.drain",
    status: "running",
    resource: { type: "replica", id },
    created_at: at,
    started_at: at,
    finished_at: null,
    scheduled_for: null,
    progress: { current: 0, total: handoffs.length, unit: "shards" },
    error: null,
    result: null,
  });
  replica.status = "draining";
  replica.drain_requested_at = at;
  replica.drain_requested_by = by;
  replica.drain_task_id = taskId;
  state.handedOff.delete(id);
  state.drains.set(id, { replica: id, startedAt: now, handoffs, taskId });
  // A replica with nothing to hand off is drained at once.
  if (handoffs.length === 0) settleCluster(now);
  return { replica: wire(replica) };
}

/** `POST /cluster/replicas/{id}/undrain`: active again, with its share back. */
export function undrainReplica(id: string, now = Date.now()): ReplicaOutcome {
  settleCluster(now);
  const replica = state.replicas.find((r) => r.id === id);
  if (!replica) return { problem: "not-found", detail: `no replica "${id}" is registered` };
  if (replica.status !== "draining" && replica.status !== "drained") {
    return { replica: wire(replica) };
  }
  const drain = state.drains.get(id);
  const handoffs = drain?.handoffs ?? state.handedOff.get(id) ?? [];
  if (drain) {
    cancelTask(drain.taskId);
    state.drains.delete(id);
  }
  // Everything it gave away (or was in the middle of giving away) comes back.
  for (const h of handoffs) {
    const shard = shardById(h.shard);
    if (h.done || shard?.state === "released") moveShard(h.shard, id);
  }
  state.handedOff.delete(id);
  replica.status = "active";
  replica.drain_requested_at = null;
  replica.drain_requested_by = null;
  replica.drain_task_id = null;
  recount(state);
  return { replica: wire(replica) };
}
