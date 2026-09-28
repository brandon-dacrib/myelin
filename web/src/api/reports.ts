/**
 * Reports (`GET /reports`, `GET /reports/{id}`, `POST /reports/{id}/resolve`,
 * `DELETE /reports/{id}`): what users flagged through the client-server API (an event, a whole
 * room, or a user), kept for moderators by `crates/hs-room/src/reports.rs` and served by
 * `crates/hs-admin/src/reports.rs`. Reads need `moderation:read`, writes `moderation:write`.
 *
 * Resolving with `no_action` dismisses a report; any other resolution resolves it. Either
 * closes it for good (the server answers `409` for a second attempt), which is why the page
 * shows the decision on the report rather than offering to change it.
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import type { components, operations } from "./schema";
import { hasScope } from "@/lib/auth";

export type Report = components["schemas"]["Report"];
export type ReportResolve = components["schemas"]["ReportResolve"];
export type ReportResolution = ReportResolve["resolution"];
export type ReportStatus = Report["status"];
export type ReportKind = Report["kind"];

type ReportListQuery = NonNullable<operations["reports.list"]["parameters"]["query"]>;

export interface ReportFilters {
  status?: ReportListQuery["status"];
  kind?: ReportListQuery["kind"];
  room_id?: string;
  sort?: string;
  cursor?: string;
  limit?: number;
}

export function useReports(filters: ReportFilters) {
  return useQuery({
    queryKey: ["reports", filters],
    enabled: hasScope("moderation:read"),
    queryFn: async () => {
      const result = await api.GET("/reports", { params: { query: filters } });
      return unwrap(result);
    },
    refetchInterval: 30_000,
  });
}

export function useReport(id: string) {
  return useQuery({
    queryKey: ["report", id],
    enabled: hasScope("moderation:read"),
    queryFn: async () => {
      const result = await api.GET("/reports/{id}", { params: { path: { id } } });
      return unwrap(result);
    },
  });
}

function invalidateReports(qc: ReturnType<typeof useQueryClient>, id: string) {
  qc.invalidateQueries({ queryKey: ["reports"] });
  qc.invalidateQueries({ queryKey: ["report", id] });
  // The Overview's pending count and attention row read this.
  qc.invalidateQueries({ queryKey: ["statistics-overview"] });
}

/** Closes a report with a resolution and an optional note (`POST /reports/{id}/resolve`). */
export function useResolveReport() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ id, ...body }: ReportResolve & { id: string }) => {
      const result = await api.POST("/reports/{id}/resolve", {
        params: { path: { id }, header: { "Idempotency-Key": newIdempotencyKey() } },
        body,
      });
      return unwrap(result);
    },
    onSuccess: (report, { id }) => {
      qc.setQueryData(["report", id], (old: Report | undefined) =>
        old ? { ...report, event: report.event ?? old.event } : report,
      );
      invalidateReports(qc, id);
    },
    // A 409 means somebody else decided it first: show what they decided.
    onError: (_error, { id }) => invalidateReports(qc, id),
  });
}

/** Deletes a report outright (`DELETE /reports/{id}`), for one filed in error or as abuse. */
export function useDeleteReport() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      const result = await api.DELETE("/reports/{id}", { params: { path: { id } } });
      if (result.error !== undefined) unwrap(result);
    },
    onSuccess: (_data, id) => {
      qc.removeQueries({ queryKey: ["report", id] });
      invalidateReports(qc, id);
    },
  });
}
