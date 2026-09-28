/**
 * Statistics beyond the Overview's counts (`crates/hs-admin/src/statistics.rs`), all
 * `admin:read`:
 *
 * - `GET /statistics/rooms`: rooms by joined members or state events (`-joined_members_count`
 *   by default).
 * - `GET /statistics/users/media`: local users by the media they uploaded (`-media_bytes` by
 *   default).
 * - `GET /statistics/timeseries?metric=`: one metric over time. Counters (`users.registered`,
 *   `media.uploaded`, `media.uploaded_bytes`, `reports.received`) count what happened in each
 *   step and have a point, possibly `0`, for every step. Gauges (the `StatisticsOverview` field
 *   names) are the server's own samples, taken every 15 minutes, and a step with no sample has
 *   no point: a new server has a short history, and the chart shows that rather than zeros.
 */
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { api } from "./client";
import { unwrap } from "./problem";
import type { components, operations } from "./schema";
import { hasScope } from "@/lib/auth";

export type Timeseries = components["schemas"]["Timeseries"];
export type TimeseriesPoint = components["schemas"]["TimeseriesPoint"];
export type RoomStatistic = components["schemas"]["RoomStatistic"];
export type UserMediaStatistic = components["schemas"]["UserMediaStatistic"];
export type Metric = operations["statistics.timeseries"]["parameters"]["query"]["metric"];

/** The ranges the Statistics page and the Overview offer, each with a step that gives a readable number of points. */
export const RANGES = {
  "24h": { label: "Last 24 hours", ms: 24 * 3_600_000, step: "1h" },
  "7d": { label: "Last 7 days", ms: 7 * 86_400_000, step: "6h" },
  "30d": { label: "Last 30 days", ms: 30 * 86_400_000, step: "1d" },
  "90d": { label: "Last 90 days", ms: 90 * 86_400_000, step: "1d" },
} as const;

export type RangeId = keyof typeof RANGES;

export function isRangeId(value: unknown): value is RangeId {
  return typeof value === "string" && value in RANGES;
}

/** Counters sum happenings per step; every other metric is a sampled gauge. */
export const COUNTER_METRICS: readonly Metric[] = [
  "users.registered",
  "media.uploaded",
  "media.uploaded_bytes",
  "reports.received",
];

export function isCounter(metric: Metric): boolean {
  return COUNTER_METRICS.includes(metric);
}

export function useTimeseries(metric: Metric, range: RangeId, options?: { enabled?: boolean }) {
  return useQuery({
    queryKey: ["statistics-timeseries", metric, range],
    enabled: hasScope("admin:read") && (options?.enabled ?? true),
    // The window is computed when the request is made rather than in the key, so a poll moves
    // it forward and the key stays stable.
    queryFn: async () => {
      const { ms, step } = RANGES[range];
      const until = new Date();
      const from = new Date(until.getTime() - ms);
      const result = await api.GET("/statistics/timeseries", {
        params: {
          query: { metric, step, from: from.toISOString(), until: until.toISOString() },
        },
      });
      return unwrap(result);
    },
    placeholderData: keepPreviousData,
    refetchInterval: 5 * 60_000,
  });
}

export interface StatisticsPageQuery {
  sort?: string;
  cursor?: string;
  limit?: number;
}

export function useRoomStatistics(query: StatisticsPageQuery) {
  return useQuery({
    queryKey: ["statistics-rooms", query],
    enabled: hasScope("admin:read"),
    queryFn: async () => {
      const result = await api.GET("/statistics/rooms", { params: { query } });
      return unwrap(result);
    },
    placeholderData: keepPreviousData,
  });
}

export function useUserMediaStatistics(query: StatisticsPageQuery) {
  return useQuery({
    queryKey: ["statistics-users-media", query],
    enabled: hasScope("admin:read"),
    queryFn: async () => {
      const result = await api.GET("/statistics/users/media", { params: { query } });
      return unwrap(result);
    },
    placeholderData: keepPreviousData,
  });
}
