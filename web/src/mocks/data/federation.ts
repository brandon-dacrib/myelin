import type { components } from "@/api/schema";
import { federationDestinations } from "./dashboard";
import { getTask, putTask } from "./tasks";

type DestinationRoom = components["schemas"]["DestinationRoom"];
type SigningKey = components["schemas"]["ServerSigningKey"];
type RemoteServerKeys = components["schemas"]["RemoteServerKeys"];

/**
 * The Federation area beyond the destination records (`crates/hs-admin/src/federation.rs`):
 * the rooms shared with each destination, this server's own signing keys, the key cache, and a
 * refetch that runs as a task (`federation.refetch_keys`) and fails for a server that is down.
 */

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
    resource: { type: "server", id: server },
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
