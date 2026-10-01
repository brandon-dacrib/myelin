/** Federation destinations (flows.md flow 4). List/summary hooks live in api/dashboard.ts (shared with the Overview page). */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import { useConfigSection } from "./config";
import { DEFAULT_MAX_QUEUED_PDUS, MAX_QUEUED_PDUS_SETTING } from "@/lib/federation";
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
