/**
 * The moderation-and-activity half of a user (`crates/hs-admin/openapi/openapi.yaml`, tag
 * Users): shadow-bans, a per-user rate-limit override, support sign-in ("login as"), redacting
 * everything they sent, deleting everything they uploaded, and what they have been doing
 * (sessions, memberships, statistics, media). Suspension itself lives in `./users` beside lock.
 *
 * Kept apart from `./users` so the account-data half of the user page can grow there without
 * the two colliding.
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import { rememberTask } from "./task-cache";
import type { components, operations } from "./schema";
import type { Task } from "./tasks";

export type RateLimitOverride = components["schemas"]["RateLimitOverride"];
export type Session = components["schemas"]["Session"];
export type Membership = components["schemas"]["RoomMember"];
export type MembershipState = NonNullable<Membership["membership"]>;
export type UserMediaItem = components["schemas"]["MediaItem"];
export type UserStatistics =
  operations["users.statistics.get"]["responses"]["200"]["content"]["application/json"];
export type LoginAsToken =
  operations["users.login_as"]["responses"]["201"]["content"]["application/json"];

/** The server's `burst_count` when an override leaves it out. */
export const DEFAULT_BURST_COUNT = 10;

function invalidateUser(qc: ReturnType<typeof useQueryClient>, userId: string) {
  qc.invalidateQueries({ queryKey: ["users"] });
  qc.invalidateQueries({ queryKey: ["user", userId] });
}

function useShadowBanAction(path: "/users/{user_id}/shadow-ban" | "/users/{user_id}/unshadow-ban") {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ userId, reason }: { userId: string; reason?: string }) => {
      const result = await api.POST(path, {
        params: { path: { user_id: userId }, header: { "Idempotency-Key": newIdempotencyKey() } },
        body: { reason: reason || undefined, notify: false },
      });
      return unwrap(result);
    },
    onSuccess: (user, { userId }) => {
      qc.setQueryData(["user", userId], user);
      invalidateUser(qc, userId);
    },
  });
}

/** `POST /users/{user_id}/shadow-ban`: what they send looks sent to them and reaches nobody. */
export const useShadowBanUser = () => useShadowBanAction("/users/{user_id}/shadow-ban");
/** `POST /users/{user_id}/unshadow-ban`. */
export const useUnshadowBanUser = () => useShadowBanAction("/users/{user_id}/unshadow-ban");

/** Whether an override is set at all: the server answers `{}` when there is none. */
export function hasRateLimitOverride(o: RateLimitOverride | undefined): o is RateLimitOverride {
  return o?.messages_per_second != null;
}

/** "Exempt", "2 messages a second, bursts of 10", as an operator would read an override. */
export function describeRateLimit(o: RateLimitOverride): string {
  const rate = o.messages_per_second ?? 0;
  if (rate === 0) return "Exempt from message rate limits";
  const burst = o.burst_count ?? DEFAULT_BURST_COUNT;
  const per = rate === 1 ? "1 message a second" : `${rate.toLocaleString()} messages a second`;
  return `${per}, bursts of ${burst.toLocaleString()}`;
}

export function useUserRateLimit(userId: string | undefined) {
  return useQuery({
    queryKey: ["user-rate-limit", userId],
    enabled: Boolean(userId),
    queryFn: async () => {
      const result = await api.GET("/users/{user_id}/rate-limit", {
        params: { path: { user_id: userId! } },
      });
      return unwrap(result);
    },
  });
}

/** `PUT /users/{user_id}/rate-limit`. A refusal names the field in `errors[].pointer`. */
export function useSetUserRateLimit() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ userId, override }: { userId: string; override: RateLimitOverride }) => {
      const result = await api.PUT("/users/{user_id}/rate-limit", {
        params: { path: { user_id: userId } },
        body: override,
      });
      return unwrap(result);
    },
    onSuccess: (saved, { userId }) => qc.setQueryData(["user-rate-limit", userId], saved),
  });
}

/** `DELETE /users/{user_id}/rate-limit`: back to the server's own limits. */
export function useClearUserRateLimit() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ userId }: { userId: string }) => {
      const result = await api.DELETE("/users/{user_id}/rate-limit", {
        params: { path: { user_id: userId } },
      });
      unwrap(result);
    },
    onSuccess: (_data, { userId }) => qc.setQueryData(["user-rate-limit", userId], {}),
  });
}

/**
 * `POST /users/{user_id}/login-as`: mints a session acting as the user. The token is in this
 * answer only; the caller shows it once and forgets it.
 */
export function useLoginAs() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      userId,
      reason,
      validForSeconds,
    }: {
      userId: string;
      reason?: string;
      validForSeconds: number;
    }): Promise<LoginAsToken> => {
      const result = await api.POST("/users/{user_id}/login-as", {
        params: { path: { user_id: userId }, header: { "Idempotency-Key": newIdempotencyKey() } },
        body: { reason: reason || undefined, valid_for_seconds: validForSeconds },
      });
      return unwrap(result);
    },
    onSuccess: (_data, { userId }) => {
      qc.invalidateQueries({ queryKey: ["user-sessions", userId] });
      qc.invalidateQueries({ queryKey: ["user-devices", userId] });
      qc.invalidateQueries({ queryKey: ["user-statistics", userId] });
    },
  });
}

export function useUserSessions(userId: string | undefined) {
  return useQuery({
    queryKey: ["user-sessions", userId],
    enabled: Boolean(userId),
    queryFn: async () => {
      const result = await api.GET("/users/{user_id}/sessions", {
        params: { path: { user_id: userId! }, query: { limit: 50 } },
      });
      return unwrap(result);
    },
  });
}

export function useUserMemberships(userId: string | undefined, membership?: MembershipState) {
  return useQuery({
    queryKey: ["user-memberships", userId, membership ?? null],
    enabled: Boolean(userId),
    queryFn: async () => {
      const result = await api.GET("/users/{user_id}/memberships", {
        params: { path: { user_id: userId! }, query: { limit: 100, membership } },
      });
      return unwrap(result);
    },
  });
}

export function useUserStatistics(userId: string | undefined) {
  return useQuery({
    queryKey: ["user-statistics", userId],
    enabled: Boolean(userId),
    queryFn: async () => {
      const result = await api.GET("/users/{user_id}/statistics", {
        params: { path: { user_id: userId! } },
      });
      return unwrap(result);
    },
  });
}

export function useUserMedia(userId: string | undefined) {
  return useQuery({
    queryKey: ["user-media", userId],
    enabled: Boolean(userId),
    queryFn: async () => {
      const result = await api.GET("/users/{user_id}/media", {
        params: { path: { user_id: userId! }, query: { limit: 50, include_total: true } },
      });
      return unwrap(result);
    },
  });
}

/** What `user.redact_events`'s finished task reports. */
export interface RedactResult {
  total: number;
  redacted: number;
  failed_count: number;
  failed: { event_id?: string; room_id?: string; reason?: string; error?: string }[];
}

export function redactResult(task: Pick<Task, "result">): RedactResult {
  const r = (task.result ?? {}) as Partial<RedactResult>;
  return {
    total: r.total ?? 0,
    redacted: r.redacted ?? 0,
    failed_count: r.failed_count ?? r.failed?.length ?? 0,
    failed: Array.isArray(r.failed) ? r.failed : [],
  };
}

/** What deleting a user's media reports when its task ends. */
export interface UserMediaDeleteResult {
  deleted: number;
  bytes: number;
  skipped_protected: number;
  failed: number;
}

export function userMediaDeleteResult(task: Pick<Task, "result">): UserMediaDeleteResult {
  const r = (task.result ?? {}) as Record<string, unknown>;
  const num = (v: unknown) => (typeof v === "number" ? v : Array.isArray(v) ? v.length : 0);
  return {
    deleted: num(r.deleted),
    bytes: num(r.bytes),
    skipped_protected: num(r.skipped_protected),
    failed: num(r.failed),
  };
}

/** `POST /users/{user_id}/redact-events`: a Task (`user.redact_events`) does the work. */
export function useRedactUserEvents() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({
      userId,
      roomId,
      reason,
      limit,
    }: {
      userId: string;
      roomId?: string;
      reason?: string;
      limit?: number;
    }): Promise<Task> => {
      const result = await api.POST("/users/{user_id}/redact-events", {
        params: { path: { user_id: userId }, header: { "Idempotency-Key": newIdempotencyKey() } },
        body: { room_id: roomId || undefined, reason: reason || undefined, limit },
      });
      return unwrap(result);
    },
    onSuccess: (task) => {
      rememberTask(qc, task);
      qc.invalidateQueries({ queryKey: ["tasks"] });
    },
  });
}

/** `DELETE /users/{user_id}/media`: everything they uploaded but protected items, as a Task. */
export function useDeleteUserMedia() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ userId }: { userId: string }): Promise<Task> => {
      const result = await api.DELETE("/users/{user_id}/media", {
        params: { path: { user_id: userId } },
      });
      return unwrap(result);
    },
    onSuccess: (task, { userId }) => {
      rememberTask(qc, task);
      qc.invalidateQueries({ queryKey: ["tasks"] });
      qc.invalidateQueries({ queryKey: ["user-media", userId] });
      qc.invalidateQueries({ queryKey: ["user-statistics", userId] });
      qc.invalidateQueries({ queryKey: ["media"] });
    },
  });
}
