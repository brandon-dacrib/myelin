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

/**
 * Signs one device out (`DELETE /users/{user_id}/devices/{device_id}`): its sessions stop at
 * once, the others stay. What an administrator does about a lost phone.
 */
export function useSignOutDevice() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ userId, deviceId }: { userId: string; deviceId: string }) => {
      const result = await api.DELETE("/users/{user_id}/devices/{device_id}", {
        params: { path: { user_id: userId, device_id: deviceId } },
      });
      return unwrap(result);
    },
    onSuccess: (_data, { userId }) => {
      invalidateUser(qc, userId);
      qc.invalidateQueries({ queryKey: ["user-devices", userId] });
    },
  });
}

/**
 * Sets a new password (`POST /users/{user_id}/reset-password`). Signs the user out everywhere
 * unless `logoutDevices` is false; the server applies its password policy and answers with a
 * problem naming `/password` when it refuses.
 */
export function useResetPassword() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      userId,
      password,
      logoutDevices,
    }: {
      userId: string;
      password: string;
      logoutDevices: boolean;
    }) => {
      const result = await api.POST("/users/{user_id}/reset-password", {
        params: { path: { user_id: userId } },
        body: { password, logout_devices: logoutDevices },
      });
      return unwrap(result);
    },
    onSuccess: (_data, { userId }) => {
      invalidateUser(qc, userId);
      qc.invalidateQueries({ queryKey: ["user-devices", userId] });
    },
  });
}

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

export type UserCreate = components["schemas"]["UserCreate"];

/**
 * Creates an account (`POST /users`). With registration closed -- the default -- this is how
 * anybody but the first administrator comes to have one.
 */
export function useCreateUser() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (body: UserCreate) => {
      const result = await api.POST("/users", {
        params: { header: { "Idempotency-Key": newIdempotencyKey() } },
        body,
      });
      return unwrap(result);
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ["users"] }),
  });
}
