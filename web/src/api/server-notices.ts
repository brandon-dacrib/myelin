/**
 * Server notices (`server_notices.*` in `crates/hs-admin/openapi/openapi.yaml`): a message from
 * the server itself, delivered to each recipient in their own server-notices room, and the
 * history of what was sent. Settings, Server notices, and a user's "Send notice".
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import type { components } from "./schema";

export type ServerNotice = components["schemas"]["ServerNotice"];

/** One sent notice, with what the contract leaves optional filled in. */
export interface ServerNoticeView {
  id: string;
  sender: string;
  type: string;
  body: string | null;
  recipients: string[];
  roomIds: string[];
  eventIds: string[];
  sentAt: string | null;
}

/** The text of a notice's content: its `body` when it has one. */
export function noticeBody(content: unknown): string | null {
  if (content && typeof content === "object" && "body" in content) {
    const body = (content as { body?: unknown }).body;
    return typeof body === "string" ? body : null;
  }
  return null;
}

export function toNoticeView(n: ServerNotice): ServerNoticeView {
  return {
    id: n.id ?? "",
    sender: n.sender ?? "",
    type: n.type ?? "m.room.message",
    body: noticeBody(n.content),
    recipients: n.recipients ?? [],
    roomIds: n.room_ids ?? [],
    eventIds: n.event_ids ?? [],
    sentAt: n.sent_at ?? null,
  };
}

export function useServerNotices(cursor?: string, enabled = true) {
  return useQuery({
    queryKey: ["server-notices", cursor ?? null],
    enabled,
    queryFn: async () => {
      const page = unwrap(
        await api.GET("/server-notices", { params: { query: { cursor, limit: 50 } } }),
      );
      return { items: page.items.map(toNoticeView), nextCursor: page.next_cursor ?? null };
    },
    refetchInterval: 30_000,
  });
}

/** A plain-text notice to the listed local users. */
export interface SendNoticeInput {
  recipients: string[];
  body: string;
}

export function useSendServerNotice() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ recipients, body }: SendNoticeInput) => {
      // A plain `m.text` message: what every client shows as a notice's text.
      const content = { msgtype: "m.text", body };
      const sent = unwrap(
        await api.POST("/server-notices", {
          params: { header: { "Idempotency-Key": newIdempotencyKey() } },
          body: { recipients, content, type: "m.room.message" },
        }),
      );
      return toNoticeView(sent);
    },
    onSuccess: () => qc.invalidateQueries({ queryKey: ["server-notices"] }),
  });
}
