import type { Replica, Shard, ShardKind } from "@/api/cluster";

/** How the interface words a replica's state, a shard's kind, and who owns what. */

type BadgeStatus = "success" | "warning" | "danger" | "info" | "muted" | "neutral";
export type ReplicaStatus = NonNullable<Replica["status"]>;

export const REPLICA_STATUS_META: Record<ReplicaStatus, { label: string; status: BadgeStatus }> = {
  joining: { label: "Joining", status: "info" },
  active: { label: "Active", status: "success" },
  draining: { label: "Draining", status: "warning" },
  drained: { label: "Drained", status: "muted" },
  unreachable: { label: "Unreachable", status: "danger" },
};

export function replicaStatusMeta(status: Replica["status"]): {
  label: string;
  status: BadgeStatus;
} {
  return status ? REPLICA_STATUS_META[status] : { label: "Unknown", status: "neutral" };
}

/** The layout order the server lists shards in (`GET /cluster/shards`). */
export const SHARD_KINDS: readonly ShardKind[] = [
  "room",
  "user",
  "federation",
  "appservice",
  "global",
];

export const SHARD_KIND_LABELS: Record<ShardKind, { plural: string; singular: string }> = {
  room: { plural: "Rooms", singular: "room" },
  user: { plural: "Users", singular: "user" },
  federation: { plural: "Federation destinations", singular: "federation" },
  appservice: { plural: "Appservices", singular: "appservice" },
  global: { plural: "Global", singular: "global" },
};

export function isShardKind(value: unknown): value is ShardKind {
  return typeof value === "string" && (SHARD_KINDS as readonly string[]).includes(value);
}

/** Whether a replica is one an administrator asked to drain (draining or already drained). */
export function isDrainRequested(replica: Pick<Replica, "status">): boolean {
  return replica.status === "draining" || replica.status === "drained";
}

/** Whether the server runs as one node, from `GET /cluster` or, failing that, the replicas. */
export function isSingleNode(mode: string | undefined, replicas: Replica[] | undefined): boolean {
  if (mode) return mode === "single-node";
  return Boolean(replicas?.some((r) => r.role === "single-node"));
}

/**
 * Why `replica` cannot be drained from this page, or `null` when it can. The server has the last
 * word (it answers 409 when nothing could take the shards); this only stops the interface from
 * offering a button that can only fail.
 */
export function drainBlockedReason(
  replica: Replica,
  replicas: Replica[],
  singleNode: boolean,
): string | null {
  if (singleNode) return "Nothing to drain to: this is a single node.";
  const others = replicas.filter((r) => r.id !== replica.id && r.status === "active");
  if (others.length === 0) return "No other replica is active to take its shards.";
  return null;
}

/**
 * Owner colours for the shard map, one per replica in list order. Each pair is readable against
 * the page's surface in its own theme; colour is never the only channel (every cell has a title,
 * the legend counts, and the table lists every shard).
 */
const OWNER_PALETTE = [
  "light-dark(#4f46e5, #818cf8)",
  "light-dark(#0f766e, #2dd4bf)",
  "light-dark(#b45309, #fbbf24)",
  "light-dark(#be123c, #fb7185)",
  "light-dark(#0369a1, #38bdf8)",
  "light-dark(#4d7c0f, #a3e635)",
  "light-dark(#a21caf, #e879f9)",
  "light-dark(#334155, #cbd5e1)",
];

/**
 * A colour for every owner: the replicas first, in the order they are listed, then any owner a
 * shard names that the replica list did not (a replica that has since gone away).
 */
export function ownerColours(replicas: Replica[], shards: Shard[]): Map<string, string> {
  const owners = new Map<string, string>();
  const add = (id: string | null | undefined) => {
    if (!id || owners.has(id)) return;
    owners.set(id, OWNER_PALETTE[owners.size % OWNER_PALETTE.length]);
  };
  replicas.forEach((r) => add(r.id));
  shards.forEach((s) => add(s.owner));
  return owners;
}

/** How many of `shards` each owner holds, `null` standing for "nobody". */
export function countByOwner(shards: Shard[]): Map<string | null, number> {
  const counts = new Map<string | null, number>();
  for (const s of shards) {
    const key = s.owner ?? null;
    counts.set(key, (counts.get(key) ?? 0) + 1);
  }
  return counts;
}

/** Shards grouped by kind, in layout order, leaving out kinds with none. */
export function groupByKind(shards: Shard[]): { kind: ShardKind; shards: Shard[] }[] {
  return SHARD_KINDS.map((kind) => ({
    kind,
    shards: shards.filter((s) => s.kind === kind),
  })).filter((g) => g.shards.length > 0);
}

/** "64 room shards: hs-0 owns 22, hs-1 owns 21, 21 unowned", for a screen reader. */
export function describeOwnership(label: string, shards: Shard[]): string {
  const parts = [...countByOwner(shards)].map(([owner, n]) =>
    owner === null ? `${n} unowned` : `${owner} owns ${n}`,
  );
  return `${shards.length} ${label} ${shards.length === 1 ? "shard" : "shards"}: ${parts.join(", ")}`;
}

export function plural(n: number, one: string, many: string): string {
  return `${n.toLocaleString()} ${n === 1 ? one : many}`;
}

/** "hs-0", "hs-0 and hs-2", "hs-0, hs-2 and hs-3". */
export function joinWithAnd(names: readonly string[]): string {
  if (names.length <= 1) return names.join("");
  return `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
}
