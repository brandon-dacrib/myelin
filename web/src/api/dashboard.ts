/**
 * The Overview page, reconciled 2026-09-18 against track 15's real
 * `crates/hs-admin/openapi/openapi.yaml`. There is no single `/overview`
 * resource (this track's own earlier draft invented one); the dashboard is
 * composed client-side from several real endpoints, each already scoped
 * and paginated for its own page:
 *
 * - `GET /statistics/overview` — the raw counts (users, rooms, media, DAU/MAU,
 *   failing destinations, pending reports).
 * - `GET /server` — version/build/uptime for the health tiles.
 * - `GET /cluster` — `replica_count` stands in for "single-node vs cluster"
 *   (there is no explicit boolean; `replica_count <= 1` is this track's
 *   heuristic, used by the add-bridge wizard to decide whether to offer a
 *   Kubernetes deployment card).
 * - `GET /appservices` — the bridges strip and "unhealthy bridge" attention
 *   rows (first page only, `health !== "healthy"`).
 * - `GET /federation/destinations` — the "failing over an hour" attention rows, from the
 *   failing destinations longest-failing first (`failing=true&sort=failing_since`), and the
 *   number of destinations in all (`include_total=true`); the failing count itself is the
 *   Overview's `federation_destinations_failing_count`, never a page's length.
 * - `GET /audit-log` — the five most recent entries.
 *
 * - `GET /statistics/timeseries?metric=...` — the "Activity" sparklines, through
 *   `useTimeseries` in `./statistics` (the metric vocabulary is the contract's
 *   `metric` enum).
 * - `GET /tasks?status=failed` — the "task failed" attention rows.
 */
import { keepPreviousData, useQuery } from "@tanstack/react-query";
import { api } from "./client";
import { useLiveEvents } from "./events";
import { unwrap } from "./problem";
import type { components } from "./schema";

/**
 * The Overview's counts, which the sidebar's open-report count reads too. While the event stream
 * is connected, a `report.*` event refetches them at once, and the rest (users, rooms, media) are
 * refreshed every five minutes; without it, every 30 seconds.
 */
export function useStatisticsOverview(options?: { enabled?: boolean }) {
  const live = useLiveEvents();
  return useQuery({
    queryKey: ["statistics-overview"],
    enabled: options?.enabled ?? true,
    queryFn: async () => {
      const result = await api.GET("/statistics/overview");
      return unwrap(result);
    },
    refetchInterval: live ? 5 * 60_000 : 30_000,
  });
}

export function useServerInfo() {
  return useQuery({
    queryKey: ["server-info"],
    queryFn: async () => {
      const result = await api.GET("/server");
      return unwrap(result);
    },
    staleTime: 60_000,
  });
}

export function useClusterStatus() {
  return useQuery({
    queryKey: ["cluster-status"],
    queryFn: async () => {
      const result = await api.GET("/cluster");
      return unwrap(result);
    },
    staleTime: 30_000,
  });
}

/** `GET /federation/destinations`'s query string (`federation.destinations.list`). */
export interface DestinationListQuery {
  limit?: number;
  cursor?: string;
  /** A field of `DESTINATION_SORT_FIELDS` (`@/lib/federation`), `-` in front for descending. */
  sort?: string;
  /** `true` for only the failing destinations, `false` for only the rest. */
  failing?: boolean;
  /** `false` for only the destinations this server shares no room with, `true` for the rest. */
  shares_room?: boolean;
  include_total?: boolean;
}

/**
 * One page of destinations, in the server's order: failing first, then by name, unless `sort`
 * says otherwise. Polled every 30 seconds; a page keeps showing while the next one loads.
 */
export function useFederationDestinations(query: DestinationListQuery = { limit: 50 }) {
  return useQuery({
    queryKey: ["federation-destinations", query],
    queryFn: async () => {
      const result = await api.GET("/federation/destinations", { params: { query } });
      return unwrap(result);
    },
    placeholderData: keepPreviousData,
    refetchInterval: 30_000,
  });
}

export function useRecentAuditEntries(limit = 5) {
  return useQuery({
    queryKey: ["audit-log-recent", limit],
    queryFn: async () => {
      const result = await api.GET("/audit-log", { params: { query: { limit } } });
      return unwrap(result);
    },
    refetchInterval: 30_000,
  });
}

export type ServerHealth = components["schemas"]["ServerHealth"];

/**
 * The server's own probe summary (`GET /server/health`): an overall `ok`, `degraded` or `down`,
 * and one line per check. A check whose backing source is absent is `unknown`, never `ok`, and
 * that alone makes the whole answer `degraded`: the server does not vouch for what it cannot see.
 */
export function useServerHealth() {
  return useQuery({
    queryKey: ["server-health"],
    queryFn: async () => {
      const result = await api.GET("/server/health");
      return unwrap(result);
    },
    refetchInterval: 30_000,
  });
}
