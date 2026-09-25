import { useQuery } from "@tanstack/react-query";
import { api, apiBaseUrl } from "./client";
import { ApiProblemError, unwrap } from "./problem";
import { getAccessToken, hasScope } from "@/lib/auth";

export type { AuditEntry } from "@/lib/audit";

/**
 * The audit log (`GET /audit-log`, `GET /audit-log/{id}`, `GET /audit-log/export`): every
 * mutation made through the admin API, written before its response is sent, and durable across
 * restarts on the real server (`crates/hs-cli/src/audit.rs`).
 *
 * The filters are the operation's own query parameters, name for name, so a URL on the Audit
 * log page is a request the server understands. `outcome` is the one the server reduces to a
 * boolean (`success` is any 2xx or 3xx). The dates are RFC 3339 and compared as strings on the
 * server, which is why the page sends full ISO timestamps rather than a day.
 */
export interface AuditFilters {
  actor?: string;
  action?: string;
  target_type?: string;
  target_id?: string;
  outcome?: "success" | "failure";
  recorded_after?: string;
  recorded_before?: string;
  cursor?: string;
  limit?: number;
}

export function useAuditEntries(filters: AuditFilters) {
  return useQuery({
    queryKey: ["audit-log", filters],
    enabled: hasScope("admin:read"),
    queryFn: async () => {
      const result = await api.GET("/audit-log", { params: { query: filters } });
      return unwrap(result);
    },
    refetchInterval: 30_000,
  });
}

export function useAuditEntry(id: string) {
  return useQuery({
    queryKey: ["audit-log", "entry", id],
    enabled: hasScope("admin:read"),
    queryFn: async () => {
      const result = await api.GET("/audit-log/{id}", { params: { path: { id } } });
      return unwrap(result);
    },
  });
}

export interface AuditExportRange {
  recorded_after?: string;
  recorded_before?: string;
}

/**
 * Downloads the log as NDJSON, one entry per line, the way `jq` and `grep` want it.
 *
 * The export honours the date range and nothing else: that is what the operation takes
 * (`openapi.yaml`, `audit_log.export`), so the page says so beside the button rather than
 * letting an operator believe the file matches the filtered table in front of them.
 *
 * A hand-rolled `fetch` rather than the typed client: the typed client parses every body as
 * JSON, and this one is a stream of lines. A plain `<a download>` would not do either, because
 * the token travels in a header, not a cookie.
 */
export async function downloadAuditExport(range: AuditExportRange): Promise<void> {
  const url = new URL(`${apiBaseUrl()}/audit-log/export`, window.location.href);
  if (range.recorded_after) url.searchParams.set("recorded_after", range.recorded_after);
  if (range.recorded_before) url.searchParams.set("recorded_before", range.recorded_before);

  const token = getAccessToken();
  const response = await fetch(url, {
    headers: token ? { Authorization: `Bearer ${token}` } : {},
  });
  if (!response.ok) {
    const problem = await response.json().catch(() => ({
      type: "about:blank",
      title: `Export failed with status ${response.status}`,
      status: response.status,
    }));
    throw new ApiProblemError(problem);
  }

  const blob = await response.blob();
  const objectUrl = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = objectUrl;
  anchor.download = `audit-log-${new Date().toISOString().slice(0, 10)}.ndjson`;
  anchor.click();
  URL.revokeObjectURL(objectUrl);
}
