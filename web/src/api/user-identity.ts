/**
 * The devices-and-identity half of a user's page, against `crates/hs-admin/openapi/openapi.yaml`:
 * renaming and bulk-signing-out devices, the email addresses and phone numbers bound to the
 * account, the upstream identity-provider subjects linked to it, its experimental features, and
 * read-only views of its account data and pushers.
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import type { components } from "./schema";

export type ThreePid = components["schemas"]["ThreePid"];
export type ExternalId = components["schemas"]["ExternalId"];

const path = (userId: string) => ({ user_id: userId });

function invalidateDevices(qc: ReturnType<typeof useQueryClient>, userId: string) {
  qc.invalidateQueries({ queryKey: ["user-devices", userId] });
  qc.invalidateQueries({ queryKey: ["user", userId] });
  qc.invalidateQueries({ queryKey: ["users"] });
}

/** Renames one device (`PATCH /users/{user_id}/devices/{device_id}`); `null` clears the name. */
export function useRenameDevice() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      userId,
      deviceId,
      displayName,
    }: {
      userId: string;
      deviceId: string;
      displayName: string | null;
    }) => {
      const result = await api.PATCH("/users/{user_id}/devices/{device_id}", {
        params: { path: { user_id: userId, device_id: deviceId } },
        // The contract types the field as a string; `null` is how the server is told to clear it.
        body: { display_name: displayName as string },
      });
      return unwrap(result);
    },
    onSuccess: (_data, { userId }) => invalidateDevices(qc, userId),
  });
}

/** Signs several devices out at once (`POST /users/{user_id}/devices/bulk-delete`). */
export function useSignOutDevices() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ userId, deviceIds }: { userId: string; deviceIds: string[] }) => {
      const result = await api.POST("/users/{user_id}/devices/bulk-delete", {
        params: { path: path(userId), header: { "Idempotency-Key": newIdempotencyKey() } },
        body: { device_ids: deviceIds },
      });
      return unwrap(result);
    },
    onSuccess: (_data, { userId }) => invalidateDevices(qc, userId),
  });
}

export function useUserThreepids(userId: string | undefined) {
  return useQuery({
    queryKey: ["user-threepids", userId],
    enabled: Boolean(userId),
    queryFn: async () => {
      const result = await api.GET("/users/{user_id}/threepids", {
        params: { path: path(userId!) },
      });
      return unwrap(result);
    },
  });
}

/** Binds an email address or phone number (`POST /users/{user_id}/threepids`). */
export function useAddThreepid() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ userId, threepid }: { userId: string; threepid: ThreePid }) => {
      const result = await api.POST("/users/{user_id}/threepids", {
        params: { path: path(userId), header: { "Idempotency-Key": newIdempotencyKey() } },
        body: threepid,
      });
      return unwrap(result);
    },
    onSuccess: (_data, { userId }) =>
      qc.invalidateQueries({ queryKey: ["user-threepids", userId] }),
  });
}

export function useRemoveThreepid() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      userId,
      medium,
      address,
    }: {
      userId: string;
      medium: string;
      address: string;
    }) => {
      const result = await api.DELETE("/users/{user_id}/threepids/{medium}/{address}", {
        params: { path: { user_id: userId, medium, address } },
      });
      return unwrap(result);
    },
    onSuccess: (_data, { userId }) =>
      qc.invalidateQueries({ queryKey: ["user-threepids", userId] }),
  });
}

export function useUserExternalIds(userId: string | undefined) {
  return useQuery({
    queryKey: ["user-external-ids", userId],
    enabled: Boolean(userId),
    queryFn: async () => {
      const result = await api.GET("/users/{user_id}/external-ids", {
        params: { path: path(userId!) },
      });
      return unwrap(result);
    },
  });
}

/** Links an upstream subject (`POST /users/{user_id}/external-ids`). */
export function useAddExternalId() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ userId, externalId }: { userId: string; externalId: ExternalId }) => {
      const result = await api.POST("/users/{user_id}/external-ids", {
        params: { path: path(userId), header: { "Idempotency-Key": newIdempotencyKey() } },
        body: externalId,
      });
      return unwrap(result);
    },
    onSuccess: (_data, { userId }) =>
      qc.invalidateQueries({ queryKey: ["user-external-ids", userId] }),
  });
}

export function useRemoveExternalId() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      userId,
      provider,
      externalId,
    }: {
      userId: string;
      provider: string;
      externalId: string;
    }) => {
      const result = await api.DELETE("/users/{user_id}/external-ids/{provider}/{external_id}", {
        params: { path: { user_id: userId, provider, external_id: externalId } },
      });
      return unwrap(result);
    },
    onSuccess: (_data, { userId }) =>
      qc.invalidateQueries({ queryKey: ["user-external-ids", userId] }),
  });
}

export function useUserExperimentalFeatures(userId: string | undefined) {
  return useQuery({
    queryKey: ["user-experimental-features", userId],
    enabled: Boolean(userId),
    queryFn: async () => {
      const result = await api.GET("/users/{user_id}/experimental-features", {
        params: { path: path(userId!) },
      });
      return unwrap(result);
    },
  });
}

/** Sets the flags named and leaves the rest (`PUT /users/{user_id}/experimental-features`). */
export function useSetExperimentalFeatures() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      userId,
      features,
    }: {
      userId: string;
      features: Record<string, boolean>;
    }) => {
      const result = await api.PUT("/users/{user_id}/experimental-features", {
        params: { path: path(userId) },
        body: features,
      });
      return unwrap(result);
    },
    onSuccess: (data, { userId }) => qc.setQueryData(["user-experimental-features", userId], data),
  });
}

export function useUserAccountData(userId: string | undefined) {
  return useQuery({
    queryKey: ["user-account-data", userId],
    enabled: Boolean(userId),
    queryFn: async () => {
      const result = await api.GET("/users/{user_id}/account-data", {
        params: { path: path(userId!) },
      });
      return unwrap(result);
    },
  });
}

/** One pusher as a client registered it (`GET /_matrix/client/v3/pushers`'s shape). */
export interface Pusher {
  pushkey: string;
  kind: string;
  app_id: string;
  app_display_name?: string;
  device_display_name?: string;
  lang?: string;
  data?: { url?: string; format?: string };
}

export function useUserPushers(userId: string | undefined) {
  return useQuery({
    queryKey: ["user-pushers", userId],
    enabled: Boolean(userId),
    queryFn: async () => {
      const result = await api.GET("/users/{user_id}/pushers", {
        params: { path: path(userId!), query: { limit: 100 } },
      });
      const page = unwrap(result);
      return { ...page, items: (page.items ?? []) as unknown as Pusher[] };
    },
  });
}
