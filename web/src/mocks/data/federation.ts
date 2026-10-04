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

/** `failing=true` keeps the failing destinations, `failing=false` the rest, absent keeps all. */
export function filterDestinations(
  items: readonly Destination[],
  failing: string | null,
): Destination[] {
  if (failing !== "true" && failing !== "false") return [...items];
  return items.filter((d) => Boolean(d.failing_since) === (failing === "true"));
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
};

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
  return federationDestinations.some((d) => d.server_name === server) ? [] : undefined;
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
