/** Users (flows.md flow 2), against the real `/users` resources in `crates/hs-admin/openapi/openapi.yaml`. */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import type { components } from "./schema";

export type User = components["schemas"]["User"];
export type Device = components["schemas"]["Device"];

export interface UserListFilters {
  q?: string;
  cursor?: string;
  limit?: number;
  admin?: boolean;
  locked?: boolean;
  suspended?: boolean;
  deactivated?: boolean;
}

export function useUsers(filters: UserListFilters) {
  return useQuery({
    queryKey: ["users", filters],
    queryFn: async () => {
      const result = await api.GET("/users", { params: { query: filters } });
      return unwrap(result);
    },
    refetchInterval: 30_000,
  });
}

export function useUser(userId: string | undefined) {
  return useQuery({
    queryKey: ["user", userId],
    enabled: Boolean(userId),
    queryFn: async () => {
      const result = await api.GET("/users/{user_id}", {
        params: { path: { user_id: userId! } },
      });
      return unwrap(result);
    },
  });
}

export function useUserDevices(userId: string | undefined) {
  return useQuery({
    queryKey: ["user-devices", userId],
    enabled: Boolean(userId),
    queryFn: async () => {
      const result = await api.GET("/users/{user_id}/devices", {
        params: { path: { user_id: userId! }, query: { limit: 50 } },
      });
      return unwrap(result);
    },
  });
}

function invalidateUser(qc: ReturnType<typeof useQueryClient>, userId: string) {
  qc.invalidateQueries({ queryKey: ["users"] });
  qc.invalidateQueries({ queryKey: ["user", userId] });
}

function useUserAction(
  path:
    | "/users/{user_id}/lock"
    | "/users/{user_id}/unlock"
    | "/users/{user_id}/suspend"
    | "/users/{user_id}/unsuspend"
    | "/users/{user_id}/logout",
) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ userId, reason }: { userId: string; reason?: string }) => {
      const result = await api.POST(path, {
        params: { path: { user_id: userId }, header: { "Idempotency-Key": newIdempotencyKey() } },
        body: { reason, notify: false },
      });
      return unwrap(result);
    },
    onSuccess: (_data, { userId }) => invalidateUser(qc, userId),
  });
}

export const useLockUser = () => useUserAction("/users/{user_id}/lock");
export const useUnlockUser = () => useUserAction("/users/{user_id}/unlock");
export const useSuspendUser = () => useUserAction("/users/{user_id}/suspend");
export const useUnsuspendUser = () => useUserAction("/users/{user_id}/unsuspend");
export const useLogoutUser = () => useUserAction("/users/{user_id}/logout");

export function useDeactivateUser() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      userId,
      erase,
      reason,
    }: {
      userId: string;
      erase?: boolean;
      reason?: string;
    }) => {
      const result = await api.POST("/users/{user_id}/deactivate", {
        params: { path: { user_id: userId }, header: { "Idempotency-Key": newIdempotencyKey() } },
        body: { erase, reason },
      });
      return unwrap(result);
    },
    onSuccess: (_data, { userId }) => invalidateUser(qc, userId),
  });
}
