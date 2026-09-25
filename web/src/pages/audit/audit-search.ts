/**
 * The Audit log page's filters, as they live in the URL (information-architecture.md #6: filters
 * and the pagination cursor are URL state, so a view can be shared or bookmarked). The names are
 * the operation's own query parameters, so `/audit?actor=@ops:example.org&outcome=failure` reads
 * the same in the address bar and in `GET /audit-log`.
 */
export interface AuditSearch {
  actor?: string;
  action?: string;
  target_type?: string;
  target_id?: string;
  outcome?: "success" | "failure";
  recorded_after?: string;
  recorded_before?: string;
  cursor?: string;
}

const STRING_KEYS = ["actor", "action", "target_type", "target_id", "cursor"] as const;

export function validateAuditSearch(search: Record<string, unknown>): AuditSearch {
  const result: AuditSearch = {};
  for (const key of STRING_KEYS) {
    const value = search[key];
    if (typeof value === "string" && value !== "") result[key] = value;
  }
  // Normalize shared URLs with timezone offsets to UTC for the date controls and API.
  for (const key of ["recorded_after", "recorded_before"] as const) {
    const value = search[key];
    if (typeof value === "string" && /^\d{4}-\d{2}-\d{2}T.*(?:Z|[+-]\d{2}:\d{2})$/.test(value)) {
      const date = new Date(value);
      if (Number.isFinite(date.getTime())) result[key] = date.toISOString();
    }
  }
  if (search.outcome === "success" || search.outcome === "failure") {
    result.outcome = search.outcome;
  }
  return result;
}

/** Whether anything other than the page cursor is set: what "Clear filters" would clear. */
export function hasAuditFilters(search: AuditSearch): boolean {
  return Object.entries(search).some(([key, value]) => key !== "cursor" && value !== undefined);
}
