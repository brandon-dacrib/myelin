/**
 * The Reports queue's filters as they live in the URL (information-architecture.md #6), named
 * after `GET /reports`'s own query parameters. `status` defaults to `open`, so the queue opens on
 * what needs deciding; `all` is the one value the server does not know, and is sent as no filter.
 */
export interface ReportsSearch {
  status?: "open" | "resolved" | "dismissed" | "all";
  kind?: "event" | "room" | "user";
  room_id?: string;
  sort?: "-received_at" | "received_at" | "score" | "-score";
  cursor?: string;
}

const STATUSES = ["open", "resolved", "dismissed", "all"] as const;
const KINDS = ["event", "room", "user"] as const;
const SORTS = ["-received_at", "received_at", "score", "-score"] as const;

function oneOf<T extends string>(values: readonly T[], value: unknown): T | undefined {
  return values.includes(value as T) ? (value as T) : undefined;
}

export function validateReportsSearch(search: Record<string, unknown>): ReportsSearch {
  return {
    status: oneOf(STATUSES, search.status),
    kind: oneOf(KINDS, search.kind),
    room_id: typeof search.room_id === "string" && search.room_id ? search.room_id : undefined,
    sort: oneOf(SORTS, search.sort),
    cursor: typeof search.cursor === "string" && search.cursor ? search.cursor : undefined,
  };
}

/** The `GET /reports` query for a URL's filters. */
export function reportsQuery(search: ReportsSearch) {
  const status = search.status ?? "open";
  return {
    status: status === "all" ? undefined : status,
    kind: search.kind,
    room_id: search.room_id,
    sort: search.sort,
    cursor: search.cursor,
    limit: 25,
  };
}
