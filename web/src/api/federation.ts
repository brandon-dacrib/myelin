/** Federation destinations (flows.md flow 4). List/summary hooks live in api/dashboard.ts (shared with the Overview page). */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import { useConfigSection } from "./config";
import {
  DEFAULT_FORGET_AFTER,
  DEFAULT_MAX_QUEUED_PDUS,
  FORGET_AFTER_SETTING,
  MAX_QUEUED_PDUS_SETTING,
  formatSettingDuration,
} from "@/lib/federation";
import { rememberTask } from "./task-cache";
import type { components } from "./schema";

export type Destination = components["schemas"]["Destination"];

export function useFederationDestination(serverName: string | undefined) {
  return useQuery({
    queryKey: ["federation-destination", serverName],
    enabled: Boolean(serverName),
    queryFn: async () => {
      const result = await api.GET("/federation/destinations/{server_name}", {
        params: { path: { server_name: serverName! } },
      });
      return unwrap(result);
    },
    refetchInterval: 15_000,
  });
}

export function useResetFederationDestination() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (serverName: string) => {
      const result = await api.POST("/federation/destinations/{server_name}/reset", {
        params: {
          path: { server_name: serverName },
          header: { "Idempotency-Key": newIdempotencyKey() },
        },
      });
      return unwrap(result);
    },
    onSuccess: (_data, serverName) => {
      qc.invalidateQueries({ queryKey: ["federation-destinations"] });
      qc.invalidateQueries({ queryKey: ["federation-destination", serverName] });
    },
  });
}

export type DestinationForgotten = components["schemas"]["DestinationForgotten"];
export type DestinationPruneReport = components["schemas"]["DestinationPruneReport"];
export type DestinationPruneEntry = components["schemas"]["DestinationPruneEntry"];

/**
 * `DELETE /federation/destinations/{server_name}`: forgets the destination (its queue, backoff,
 * catch-up mark and cached keys). The server answers `409 conflict` while it shares a room with
 * this one unless `force` is given; the caller shows that refusal and offers the force.
 */
export function useForgetDestination() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      serverName,
      force,
    }: {
      serverName: string;
      force?: boolean;
    }): Promise<DestinationForgotten> => {
      const result = await api.DELETE("/federation/destinations/{server_name}", {
        params: {
          path: { server_name: serverName },
          query: force ? { force: true } : {},
          header: { "Idempotency-Key": newIdempotencyKey() },
        },
      });
      return unwrap(result);
    },
    onSuccess: (_data, { serverName }) => {
      void qc.invalidateQueries({ queryKey: ["federation-destinations"] });
      void qc.invalidateQueries({ queryKey: ["federation-destination", serverName] });
      void qc.invalidateQueries({ queryKey: ["federation-remote-keys", serverName] });
    },
  });
}

/**
 * `POST /federation/destinations/prune`: forgets every destination sharing no room with
 * nothing queued and, with `failingFor` (`"7d"`), every one failing that long whose queue is
 * only for rooms this server left. `dryRun` answers the same report and forgets nothing.
 */
export function usePruneDestinations() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      dryRun,
      failingFor,
    }: {
      dryRun: boolean;
      failingFor?: string;
    }): Promise<DestinationPruneReport> => {
      const result = await api.POST("/federation/destinations/prune", {
        params: {
          query: dryRun ? { dry_run: true } : {},
          header: { "Idempotency-Key": newIdempotencyKey() },
        },
        body: failingFor ? { failing_for: failingFor } : {},
      });
      return unwrap(result);
    },
    onSuccess: (_report, { dryRun }) => {
      if (!dryRun) void qc.invalidateQueries({ queryKey: ["federation-destinations"] });
    },
  });
}

export type DestinationRoom = components["schemas"]["DestinationRoom"];
export type SigningKey = components["schemas"]["ServerSigningKey"];
export type RemoteServerKeys = components["schemas"]["RemoteServerKeys"];
type Task = components["schemas"]["Task"];

/**
 * `GET /federation/destinations/{server_name}/rooms`: the rooms this server shares with a
 * destination, the ones with most of its users first (first 100).
 */
export function useDestinationRooms(serverName: string | undefined) {
  return useQuery({
    queryKey: ["federation-destination-rooms", serverName],
    enabled: Boolean(serverName),
    queryFn: async () => {
      const result = await api.GET("/federation/destinations/{server_name}/rooms", {
        params: { path: { server_name: serverName! }, query: { limit: 100, include_total: true } },
      });
      return unwrap(result);
    },
  });
}

/** `GET /federation/keys`: this server's own signing keys. */
export function useOwnSigningKeys() {
  return useQuery({
    queryKey: ["federation-keys"],
    queryFn: async () => unwrap(await api.GET("/federation/keys")),
  });
}

/**
 * `GET /federation/keys/{server_name}`: what this server's key cache holds for another server,
 * or `null` when it holds nothing (a `404`: it has not needed one of that server's signatures).
 */
export function useRemoteKeys(serverName: string | undefined) {
  return useQuery({
    queryKey: ["federation-remote-keys", serverName],
    enabled: Boolean(serverName),
    queryFn: async (): Promise<RemoteServerKeys | null> => {
      const result = await api.GET("/federation/keys/{server_name}", {
        params: { path: { server_name: serverName! } },
      });
      if (result.error?.status === 404) return null;
      return unwrap(result);
    },
  });
}

/**
 * `POST /federation/keys/{server_name}/refresh`: fetches that server's keys again. Answered
 * with the task that does it (`federation.refetch_keys`); follow it with `useTask`.
 */
export function useRefreshRemoteKeys() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (serverName: string): Promise<Task> => {
      const result = await api.POST("/federation/keys/{server_name}/refresh", {
        params: {
          path: { server_name: serverName },
          header: { "Idempotency-Key": newIdempotencyKey() },
        },
      });
      return unwrap(result);
    },
    onSuccess: (task) => {
      rememberTask(qc, task);
      void qc.invalidateQueries({ queryKey: ["tasks"] });
    },
  });
}

/**
 * The queue limit this server runs with: the configured value when the `federation` section
 * can be read, else the default.
 */
export function useFederationQueueLimit(): { limit: number; configured: boolean } {
  const { data } = useConfigSection("federation");
  const value = data?.section.values?.[MAX_QUEUED_PDUS_SETTING];
  return typeof value === "number"
    ? { limit: value, configured: value !== DEFAULT_MAX_QUEUED_PDUS }
    : { limit: DEFAULT_MAX_QUEUED_PDUS, configured: false };
}

/**
 * How long the hourly sweep keeps a destination this server shares no room with
 * (`federation.forget_unused_destinations_after`): the configured value when the `federation`
 * section can be read (`null` when it is `0`: the sweep is off), else the default, `"1w"`.
 */
export function useForgetAfterSetting(): {
  /** The duration in words ("1 week"), or `null` when the sweep is off. */
  after: string | null;
  /** Whether this is the configured value rather than the default. */
  configured: boolean;
} {
  const { data } = useConfigSection("federation");
  const value = data?.section.values?.[FORGET_AFTER_SETTING];
  const configured = formatSettingDuration(value);
  if (configured !== undefined) return { after: configured, configured: true };
  return {
    after: formatSettingDuration(DEFAULT_FORGET_AFTER) ?? DEFAULT_FORGET_AFTER,
    configured: false,
  };
}
