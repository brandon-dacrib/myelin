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
 * - `GET /federation/destinations` — the federation strip and "failing over
 *   an hour" attention rows.
 * - `GET /audit-log` — the five most recent entries.
 *
 * The richer "Activity" sparklines (`GET /statistics/timeseries?metric=...`)
 * are deliberately not wired in yet: the metric-name vocabulary isn't
 * documented in the schema, so a real fixture/backend is needed to know
 * what to ask for. Left for the next session; see
 * docs/status/16-management-web-interface.md.
 */
import { useQuery } from "@tanstack/react-query";
import { api } from "./client";

export function useStatisticsOverview() {
  return useQuery({
    queryKey: ["statistics-overview"],
    queryFn: async () => {
      const { data, error } = await api.GET("/statistics/overview");
      if (error) throw error;
      return data;
    },
    refetchInterval: 30_000,
  });
}

export function useServerInfo() {
  return useQuery({
    queryKey: ["server-info"],
    queryFn: async () => {
      const { data, error } = await api.GET("/server");
      if (error) throw error;
      return data;
    },
    staleTime: 60_000,
  });
}

export function useClusterStatus() {
  return useQuery({
    queryKey: ["cluster-status"],
    queryFn: async () => {
      const { data, error } = await api.GET("/cluster");
      if (error) throw error;
      return data;
    },
    staleTime: 30_000,
  });
}

export function useFederationDestinations(limit = 50) {
  return useQuery({
    queryKey: ["federation-destinations", limit],
    queryFn: async () => {
      const { data, error } = await api.GET("/federation/destinations", {
        params: { query: { limit } },
      });
      if (error) throw error;
      return data;
    },
    refetchInterval: 30_000,
  });
}

export function useRecentAuditEntries(limit = 5) {
  return useQuery({
    queryKey: ["audit-log-recent", limit],
    queryFn: async () => {
      const { data, error } = await api.GET("/audit-log", { params: { query: { limit } } });
      if (error) throw error;
      return data;
    },
    refetchInterval: 30_000,
  });
}
