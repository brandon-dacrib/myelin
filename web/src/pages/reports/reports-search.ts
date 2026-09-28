/**
 * The Reports queue's filters as they live in the URL (information-architecture.md #6), named
 * after `GET /reports`'s own query parameters. `status` defaults to `open`, so the queue opens on
 * what needs deciding; `all` is the one value the server does not know, and is sent as no filter.
 */
export interface ReportsSearch {
  status?: "open" | "resolved" | "dismissed" | "all";
  kind?: "event" | "room" | "user";
  room_id?: string;
  /** Reports about this person's conduct. */
  reported_user_id?: string;
  /** Reports this person filed. */
  reporter_id?: string;
  sort?: "-received_at" | "received_at" | "score" | "-score";
  cursor?: string;
}

const STATUSES = ["open", "resolved", "dismissed", "all"] as const;
const KINDS = ["event", "room", "user"] as const;
const SORTS = ["-received_at", "received_at", "score", "-score"] as const;

function oneOf<T extends string>(values: readonly T[], value: unknown): T | undefined {
  return values.includes(value as T) ? (value as T) : undefined;
}

function nonEmpty(value: unknown): string | undefined {
  return typeof value === "string" && value ? value : undefined;
}

export function validateReportsSearch(search: Record<string, unknown>): ReportsSearch {
  return {
    status: oneOf(STATUSES, search.status),
    kind: oneOf(KINDS, search.kind),
    room_id: nonEmpty(search.room_id),
    reported_user_id: nonEmpty(search.reported_user_id),
    reporter_id: nonEmpty(search.reporter_id),
    sort: oneOf(SORTS, search.sort),
    cursor: nonEmpty(search.cursor),
  };
}

/** The `GET /reports` query for a URL's filters. */
export function reportsQuery(search: ReportsSearch) {
  const status = search.status ?? "open";
  return {
    status: status === "all" ? undefined : status,
    kind: search.kind,
    room_id: search.room_id,
    reported_user_id: search.reported_user_id,
    reporter_id: search.reporter_id,
    sort: search.sort,
    cursor: search.cursor,
    limit: 25,
  };
}
