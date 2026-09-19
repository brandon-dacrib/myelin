/** Federation destinations (flows.md flow 4). List/summary hooks live in api/dashboard.ts (shared with the Overview page). */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
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
