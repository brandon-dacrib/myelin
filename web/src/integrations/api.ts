/**
 * The user webhooks API (`crates/hs-bridges/src/user_webhooks.rs`, decision 0057):
 * `/_matrix/client/unstable/io.myelin.webhooks`, called with the signed-in person's own
 * token. Plain `fetch`, not the admin API's generated client: this API is not part of the admin
 * OpenAPI (it is a client-server extension), and its errors are Matrix errors
 * (`{errcode, error}`), not RFC 9457 problems.
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import type { BridgeConnectionCreated, BridgeWebhookSource } from "@/api/bridges";
import { forgetSession, getUserSession } from "./auth";

export const USER_WEBHOOKS_PREFIX = "/_matrix/client/unstable/io.myelin.webhooks";

/** A Matrix error answer (`{errcode, error, retry_after_ms?}`), with its status. */
export class MatrixApiError extends Error {
  readonly status: number;
  readonly errcode: string;
  readonly retryAfterMs: number | undefined;

  constructor(status: number, errcode: string, error: string, retryAfterMs?: number) {
    super(error);
    this.name = "MatrixApiError";
    this.status = status;
    this.errcode = errcode;
    this.retryAfterMs = retryAfterMs;
  }
}

export interface UserRoom {
  room_id: string;
  name?: string | null;
  bot_joined: boolean;
  may_add: boolean;
  reason?: string | null;
}

export interface UserWebhook {
  room_id: string;
  room_name?: string | null;
  name: string;
  source?: string | null;
  created_by?: string | null;
  created_at?: number | null;
}

export interface WebhookCreate {
  source: string;
  name: string;
  room: { id: string } | { create: { name: string; topic?: string } };
}

export type { BridgeConnectionCreated as WebhookCreated, BridgeWebhookSource as WebhookSource };

async function call<T>(method: string, path: string, body?: unknown): Promise<T> {
  const session = getUserSession();
  const headers: Record<string, string> = { Accept: "application/json" };
  if (session) headers["Authorization"] = `Bearer ${session.accessToken}`;
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const res = await fetch(`${USER_WEBHOOKS_PREFIX}${path}`, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  if (res.ok) {
    return (await res.json()) as T;
  }
  const answer = (await res.json().catch(() => ({}))) as {
    errcode?: string;
    error?: string;
    retry_after_ms?: number;
  };
  if (res.status === 401) {
    // The token is gone (signed out elsewhere, expired): back to the sign-in.
    forgetSession();
  }
  throw new MatrixApiError(
    res.status,
    answer.errcode ?? "M_UNKNOWN",
    answer.error ?? `The server answered ${res.status}.`,
    answer.retry_after_ms,
  );
}

export function useWebhookSources(enabled: boolean) {
  return useQuery({
    queryKey: ["integrations", "sources"],
    queryFn: () => call<{ items: BridgeWebhookSource[] }>("GET", "/sources").then((r) => r.items),
    enabled,
    staleTime: 5 * 60_000,
  });
}

export function useMyRooms(enabled: boolean) {
  return useQuery({
    queryKey: ["integrations", "rooms"],
    queryFn: () => call<{ rooms: UserRoom[] }>("GET", "/rooms").then((r) => r.rooms),
    enabled,
  });
}

export function useMyWebhooks(enabled: boolean) {
  return useQuery({
    queryKey: ["integrations", "webhooks"],
    queryFn: () => call<{ items: UserWebhook[] }>("GET", "/webhooks").then((r) => r.items),
    enabled,
  });
}

export function useCreateWebhook() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (body: WebhookCreate) => call<BridgeConnectionCreated>("POST", "/webhooks", body),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ["integrations", "webhooks"] });
      void queryClient.invalidateQueries({ queryKey: ["integrations", "rooms"] });
    },
  });
}

export function useRemoveWebhook() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ roomId, name }: { roomId: string; name: string }) =>
      call<Record<string, never>>(
        "DELETE",
        `/webhooks/${encodeURIComponent(roomId)}/${encodeURIComponent(name)}`,
      ),
    onSuccess: () => {
      void queryClient.invalidateQueries({ queryKey: ["integrations", "webhooks"] });
    },
  });
}

/** The error's message for a person, from a {@link MatrixApiError} or anything else thrown. */
export function describeError(err: unknown): string {
  if (err instanceof MatrixApiError) {
    if (err.errcode === "M_LIMIT_EXCEEDED") {
      const seconds = Math.ceil((err.retryAfterMs ?? 0) / 1000);
      return seconds > 0
        ? `You have made a lot of webhooks in the last hour; try again in ${seconds} seconds.`
        : "You have made a lot of webhooks in the last hour; try again later.";
    }
    return err.message;
  }
  if (err instanceof Error && err.message) return err.message;
  return "Couldn't reach the server.";
}
