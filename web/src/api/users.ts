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

/**
 * Up to `limit` users matching `q`, for a picker that suggests people as an administrator types
 * (the server-notice recipients). Only asks once there is something to search for.
 */
export function useUserSuggestions(q: string, limit = 5) {
  const query = q.trim().replace(/^@/, "");
  return useQuery({
    queryKey: ["user-suggestions", query, limit],
    enabled: query.length >= 2,
    queryFn: async () => {
      const result = await api.GET("/users", { params: { query: { q: query, limit } } });
      return unwrap(result).items;
    },
    staleTime: 15_000,
    retry: false,
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

/**
 * After anything that changes an account: the lists, the account itself, and every per-user
 * query (`["user-devices", id]`, `["user-threepids", id]`, `["user-memberships", id, ...]` and
 * the rest, across the api modules), since one action can change several of them at once.
 * Erasing an account empties its devices, identities and memberships, and until 2026-10-02
 * only `["user", id]` was refetched, so the page kept showing the devices the server had just
 * deleted (found by `web/e2e-real/user-erase.spec.ts` against the real server).
 */
function invalidateUser(qc: ReturnType<typeof useQueryClient>, userId: string) {
  qc.invalidateQueries({ queryKey: ["users"] });
  qc.invalidateQueries({
    predicate: (query) => {
      const [kind, id] = query.queryKey;
      return typeof kind === "string" && kind.startsWith("user") && id === userId;
    },
  });
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

/**
 * Lets a deactivated account sign in again (`POST /users/{user_id}/reactivate`). Its password
 * and devices are as deactivation left them; rooms it left are not rejoined.
 */
export function useReactivateUser() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ userId }: { userId: string }) => {
      const result = await api.POST("/users/{user_id}/reactivate", {
        params: { path: { user_id: userId }, header: { "Idempotency-Key": newIdempotencyKey() } },
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

export type UserUpdate = components["schemas"]["UserUpdate"];

/**
 * Changes an account's own fields (`PATCH /users/{user_id}`): server administrator, display
 * name, avatar and kind of account. A name or avatar change reaches every room the user is in
 * (the server re-sends their membership). Send only the fields that changed: the server touches
 * only what a request names, and `null` clears a field.
 */
export function useUpdateUser() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ userId, patch }: { userId: string; patch: UserUpdate }) => {
      const result = await api.PATCH("/users/{user_id}", {
        params: { path: { user_id: userId } },
        body: patch,
      });
      return unwrap(result);
    },
    onSuccess: (_data, { userId }) => invalidateUser(qc, userId),
  });
}

/** What `users.lookup` can be asked for: a verified email or phone, or a sign-in provider's subject. */
export type UserLookup =
  | { kind: "email" | "msisdn"; address: string }
  | { kind: "external"; provider: string; externalId: string };

/**
 * The one account that has exactly this email, phone number or sign-in identity
 * (`GET /users/lookup`), or `null` when none has: the server's `404` is an answer here, not a
 * fault. The list's search matches names and IDs loosely; this is for when an administrator
 * holds the exact address a person signed up with.
 */
export async function lookupUser(lookup: UserLookup): Promise<User | null> {
  const query =
    lookup.kind === "external"
      ? { provider: lookup.provider, external_id: lookup.externalId }
      : { medium: lookup.kind, address: lookup.address };
  const result = await api.GET("/users/lookup", { params: { query } });
  if (result.response.status === 404) return null;
  return unwrap(result);
}

/**
 * Whether `localpart` is free (`GET /users/availability`), asked while an administrator types a
 * username. A name that could never be one is `400` with the server's reason (`param:localpart`),
 * which the caller shows; a server whose user directory cannot check in advance answers `503`,
 * and the caller says so instead of pretending to know.
 */
export function useLocalpartAvailability(localpart: string) {
  return useQuery({
    queryKey: ["localpart-availability", localpart],
    enabled: localpart.length > 0,
    queryFn: async () => {
      const result = await api.GET("/users/availability", { params: { query: { localpart } } });
      return unwrap(result).available ?? null;
    },
    staleTime: 15_000,
    retry: false,
  });
}
