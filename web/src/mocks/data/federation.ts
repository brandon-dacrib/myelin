import type { components } from "@/api/schema";
import { federationDestinations } from "./dashboard";
import { getTask, putTask } from "./tasks";

type Destination = components["schemas"]["Destination"];
type DestinationRoom = components["schemas"]["DestinationRoom"];
type SigningKey = components["schemas"]["ServerSigningKey"];
type RemoteServerKeys = components["schemas"]["RemoteServerKeys"];

/**
 * The Federation area beyond the destination records (`crates/hs-admin/src/federation.rs`):
 * the rooms shared with each destination, this server's own signing keys, the key cache, and a
 * refetch that runs as a task (`federation.refetch_keys`) and fails for a server that is down.
 */

/** The fields `GET /federation/destinations` sorts by, as `crates/hs-admin/src/router.rs` has them. */
const SORT_FIELDS = [
  "server_name",
  "failing_since",
  "last_successful_at",
  "retry_last_at",
  "pending_pdu_count",
  "pending_edu_count",
];

/**
 * `GET /federation/destinations`' order (`sort_destinations` in `crates/hs-admin/src/router.rs`):
 * without a sort, failing first then by name; with one, by that field, `-` for descending, a
 * destination without the timestamp last either way, ties by name. `null` for an unknown field.
 */
export function sortDestinations(
  items: readonly Destination[],
  sort: string | null,
): Destination[] | null {
  const sorted = [...items];
  const byName = (a: Destination, b: Destination) =>
    (a.server_name ?? "").localeCompare(b.server_name ?? "");
  const trimmed = sort?.trim();
  if (!trimmed) {
    return sorted.sort(
      (a, b) => Number(Boolean(b.failing_since)) - Number(Boolean(a.failing_since)) || byName(a, b),
    );
  }
  const descending = trimmed.startsWith("-");
  const field = descending ? trimmed.slice(1) : trimmed;
  if (!SORT_FIELDS.includes(field)) return null;
  const direction = (o: number) => (descending ? -o : o);
  const compare = (a: Destination, b: Destination): number => {
    switch (field) {
      case "server_name":
        return direction(byName(a, b));
      case "pending_pdu_count":
        return direction((a.pending_pdu_count ?? 0) - (b.pending_pdu_count ?? 0));
      case "pending_edu_count":
        return direction((a.pending_edu_count ?? 0) - (b.pending_edu_count ?? 0));
      default: {
        const key = field as "failing_since" | "last_successful_at" | "retry_last_at";
        const [x, y] = [a[key], b[key]];
        // Absent last whichever way the rest goes.
        if (!x || !y) return Number(!x) - Number(!y);
        return direction(x.localeCompare(y));
      }
    }
  };
  return sorted.sort((a, b) => compare(a, b) || byName(a, b));
}

/**
 * `failing=true` keeps the failing destinations, `failing=false` the rest; `shares_room=false`
 * keeps those sharing no room, `shares_room=true` the rest; absent keeps all.
 */
export function filterDestinations(
  items: readonly Destination[],
  failing: string | null,
  sharesRoom: string | null = null,
): Destination[] {
  let out = [...items];
  if (failing === "true" || failing === "false")
    out = out.filter((d) => Boolean(d.failing_since) === (failing === "true"));
  if (sharesRoom === "true" || sharesRoom === "false")
    out = out.filter((d) => (d.shared_rooms_count ?? 0) > 0 === (sharesRoom === "true"));
  return out;
}

/** A destination row with `shared_rooms_count`, read from the rooms shared with it below. */
export function withSharedRooms(d: Destination): Destination {
  return { ...d, shared_rooms_count: sharedRoomsCount(d.server_name ?? "") };
}

/**
 * How many rooms this server shares with `server`: the rooms below. Every eighth quiet server
 * (`srv-07`, `srv-14`, ...: eight of them) shares none, so the "No shared room" filter, a forget
 * and a prune have something to show.
 */
export function sharedRoomsCount(server: string): number {
  return (sharedRooms[server] ?? generalRoomFor(server)).length;
}

/** Whether a generated quiet server shares the General room: all but every seventh. */
function generalRoomFor(server: string): DestinationRoom[] {
  const match = /^srv-(\d+)\.example\.net$/.exec(server);
  if (!match || Number(match[1]) % 7 === 0) return [];
  return [
    {
      room_id: "!general:example.org",
      name: "General",
      canonical_alias: "#general:example.org",
      joined_members_count: 214,
      destination_members_count: 1,
    },
  ];
}

const DAY = 86_400_000;
const iso = (msFromNow: number) => new Date(Date.now() + msFromNow).toISOString();

const sharedRooms: Record<string, DestinationRoom[]> = {
  "matrix.org": [
    {
      room_id: "!general:example.org",
      name: "General",
      canonical_alias: "#general:example.org",
      joined_members_count: 214,
      destination_members_count: 37,
    },
  ],
  "element.io": [
    {
      room_id: "!general:example.org",
      name: "General",
      canonical_alias: "#general:example.org",
      joined_members_count: 214,
      destination_members_count: 5,
    },
  ],
  "gnome.org": [
    {
      room_id: "!spam-central:example.org",
      name: "spam-central",
      canonical_alias: null,
      joined_members_count: 3,
      destination_members_count: 2,
    },
  ],
  "mozilla.org": [
    {
      room_id: "!general:example.org",
      name: "General",
      canonical_alias: "#general:example.org",
      joined_members_count: 214,
      destination_members_count: 12,
    },
    {
      room_id: "!spam-central:example.org",
      name: "spam-central",
      canonical_alias: null,
      joined_members_count: 3,
      destination_members_count: 1,
    },
  ],
  "kde.org": [
    {
      room_id: "!general:example.org",
      name: "General",
      canonical_alias: "#general:example.org",
      joined_members_count: 214,
      destination_members_count: 4,
    },
  ],
};

type DestinationForgotten = components["schemas"]["DestinationForgotten"];
type DestinationPruneReport = components["schemas"]["DestinationPruneReport"];
type DestinationPruneEntry = components["schemas"]["DestinationPruneEntry"];

/**
 * `DELETE /federation/destinations/{server_name}` (`forget_destination` in
 * `crates/hs-admin/src/federation.rs`): `"not-found"` for a server never tried, `"shares-rooms"`
 * while a room is shared unless `force`, else the row is gone and what it held is reported.
 */
export function forgetDestination(
  server: string,
  force: boolean,
): DestinationForgotten | "not-found" | { shares: number } {
  const index = federationDestinations.findIndex((d) => d.server_name === server);
  if (index < 0) return "not-found";
  const d = federationDestinations[index];
  const shares = sharedRoomsCount(server);
  if (shares > 0 && !force) return { shares };
  federationDestinations.splice(index, 1);
  const keys = cachedKeys(server);
  delete cache[server];
  return {
    server_name: server,
    dropped_pdu_count: d.catch_up_since ? 0 : (d.pending_pdu_count ?? 0),
    dropped_edu_count: d.pending_edu_count ?? 0,
    dropped_key_count: keys?.keys.length ?? 0,
    was_catching_up: Boolean(d.catch_up_since),
    shared_rooms_count: shares,
  };
}

/** `"7d"` → milliseconds, as `parse_failing_for` reads it; `undefined` for anything else. */
export function parseFailingFor(text: string): number | undefined {
  const units: Record<string, number> = {
    ms: 1,
    s: 1000,
    m: 60_000,
    h: 3_600_000,
    d: 86_400_000,
    w: 7 * 86_400_000,
  };
  const trimmed = text.trim();
  if (/^\d+$/.test(trimmed)) return Number(trimmed);
  let total = 0;
  let consumed = 0;
  for (const match of trimmed.matchAll(/(\d+)(ms|s|m|h|d|w)/gy)) {
    total += Number(match[1]) * units[match[2]];
    consumed = match.index + match[0].length;
  }
  return consumed === trimmed.length && trimmed.length > 0 ? total : undefined;
}

/**
 * `POST /federation/destinations/prune` as `decide` in `crates/hs-admin/src/federation.rs`
 * rules, with the administrator's zero idle time: a server sharing a room is kept
 * (`shares_rooms`); one sharing none with nothing queued is forgotten (`unused`); one with a
 * queue is kept unless `failingFor` is given and it has failed that long (`failing`), else
 * `queued_not_failing` or `failing_recently`.
 */
export function pruneDestinations(dryRun: boolean, failingFor?: number): DestinationPruneReport {
  const forgotten: DestinationPruneEntry[] = [];
  const kept: DestinationPruneEntry[] = [];
  const now = Date.now();
  for (const d of federationDestinations) {
    const name = d.server_name ?? "";
    const shares = sharedRoomsCount(name);
    const queued = (d.catch_up_since ? 0 : (d.pending_pdu_count ?? 0)) + (d.pending_edu_count ?? 0);
    if (shares > 0) {
      kept.push({
        server_name: name,
        reason: "shares_rooms",
        detail: `shares ${shares} ${shares === 1 ? "room" : "rooms"} with this server`,
      });
    } else if (queued === 0) {
      forgotten.push({
        server_name: name,
        reason: "unused",
        detail: "shares no room and has nothing queued",
      });
    } else if (!d.failing_since) {
      kept.push({
        server_name: name,
        reason: "queued_not_failing",
        detail: `${queued} queued and not failing: they will be delivered`,
      });
    } else if (failingFor != null && now - Date.parse(d.failing_since) >= failingFor) {
      forgotten.push({
        server_name: name,
        reason: "failing",
        detail: `failing since ${d.failing_since}; its ${queued} queued are for rooms this server left`,
      });
    } else {
      kept.push({
        server_name: name,
        reason: "failing_recently",
        detail: `failing since ${d.failing_since}, not yet for long enough`,
      });
    }
  }
  if (!dryRun) {
    const gone = new Set(forgotten.map((e) => e.server_name));
    for (let i = federationDestinations.length - 1; i >= 0; i--) {
      if (gone.has(federationDestinations[i].server_name ?? ""))
        federationDestinations.splice(i, 1);
    }
    for (const name of gone) delete cache[name];
  }
  const group = (entries: DestinationPruneEntry[]) => {
    const by_reason: Record<string, number> = {};
    for (const e of entries) by_reason[e.reason] = (by_reason[e.reason] ?? 0) + 1;
    return { count: entries.length, by_reason, servers: entries.slice(0, 50) };
  };
  return { dry_run: dryRun, forgotten: group(forgotten), kept: group(kept) };
}

export const ownKeys: SigningKey[] = [
  {
    key_id: "ed25519:a_1727000000000_q2V9Zw",
    algorithm: "ed25519",
    public_key: "Xh4nTjI7xYw0Gm4bq1b3m9cQ8Hc0eQy1yKfJ0kO4t0Q",
    valid_until_at: null,
    old: false,
  },
];

function keysFor(server: string, fetchedMsAgo: number): RemoteServerKeys {
  return {
    server_name: server,
    keys: [
      {
        key_id: `ed25519:${server.split(".")[0]}_2026`,
        algorithm: "ed25519",
        public_key: btoa(`${server} signing key`).replace(/=+$/, ""),
        valid_until_at: iso(7 * DAY - fetchedMsAgo),
        old: false,
      },
    ],
    cached_at: iso(-fetchedMsAgo),
  };
}

function seedCache(): Record<string, RemoteServerKeys> {
  return {
    "matrix.org": keysFor("matrix.org", 2 * 60 * 60_000),
    "element.io": keysFor("element.io", 26 * 60 * 60_000),
  };
}

let cache = seedCache();

/** Puts the key cache back as it was (Vitest runs this after every test). */
export function resetFederationKeys(): void {
  cache = seedCache();
}

export function destinationRooms(server: string): DestinationRoom[] | undefined {
  if (sharedRooms[server]) return sharedRooms[server];
  return federationDestinations.some((d) => d.server_name === server)
    ? generalRoomFor(server)
    : undefined;
}

export function cachedKeys(server: string): RemoteServerKeys | undefined {
  return cache[server];
}

/** How long the mock takes to refetch a server's keys. */
const REFRESH_MS = 600;

/**
 * Starts a refetch of `server`'s keys: the task is answered `running`, and ends a moment later,
 * `succeeded` with the keys (now in the cache) or `failed` for a server that is failing.
 */
export function startKeyRefresh(server: string) {
  const now = new Date().toISOString();
  const id = `task_${Math.random().toString(36).slice(2, 10)}`;
  const task = putTask({
    id,
    action: "federation.refetch_keys",
    status: "running",
    resource: { type: "destination", id: server },
    progress: null,
    result: null,
    error: null,
    created_at: now,
    started_at: now,
    finished_at: null,
  });
  setTimeout(() => {
    const current = getTask(id);
    if (!current || current.status !== "running") return;
    const down = federationDestinations.find((d) => d.server_name === server)?.failing_since;
    if (down || server.endsWith(".invalid")) {
      putTask({
        ...current,
        status: "failed",
        finished_at: new Date().toISOString(),
        error: {
          type: "urn:hs:problem:unavailable",
          title: "Unavailable",
          status: 503,
          detail: `the data source is temporarily unavailable: could not fetch keys for server \`${server}\``,
        },
      });
      return;
    }
    cache[server] = keysFor(server, 0);
    putTask({
      ...current,
      status: "succeeded",
      finished_at: new Date().toISOString(),
      result: cache[server],
    });
  }, REFRESH_MS);
  return task;
}
