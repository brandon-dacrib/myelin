/**
 * The room page's long tail (the seventeen room operations beyond list, get, block, unblock,
 * make-admin and members), against `crates/hs-admin/openapi/openapi.yaml`.
 *
 * Reads of message content (`rooms.messages.list`, `rooms.events.*`, `events.get`) need
 * `admin:read`, and the server records each one in the audit log as `rooms.content.read`
 * (decision 0013); the room's structure (state, aliases, hierarchy, media) is readable with
 * `moderation:read`. Purge, delete and media quarantine answer `202` with a Task that the page
 * follows through `useTask`.
 */
import { useInfiniteQuery, useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import type { components } from "./schema";

export type RoomEvent = components["schemas"]["RoomEvent"];
export type StateEvent = components["schemas"]["StateEvent"];
export type EventContext = components["schemas"]["EventContext"];
export type RoomAlias = components["schemas"]["RoomAlias"];
export type RoomHierarchyNode = components["schemas"]["RoomHierarchyNode"];
export type ForwardExtremity = components["schemas"]["ForwardExtremity"];
export type ForwardExtremitiesPruned = components["schemas"]["ForwardExtremitiesPruned"];
export type RoomTask = components["schemas"]["Task"];
export type RoomMediaItem = components["schemas"]["MediaItem"];

const ROOM_KEYS = [
  "room",
  "room-members",
  "room-state",
  "room-messages",
  "room-aliases",
  "room-hierarchy",
  "room-extremities",
  "room-media",
] as const;

/** Refreshes everything the room page shows about `roomId`, and the rooms list. */
function invalidateRoomContents(qc: ReturnType<typeof useQueryClient>, roomId: string) {
  qc.invalidateQueries({ queryKey: ["rooms"] });
  for (const key of ROOM_KEYS) qc.invalidateQueries({ queryKey: [key, roomId] });
}

/** For a page that saw a room task end: refresh the room. */
export function useRefreshRoom() {
  const qc = useQueryClient();
  return (roomId: string) => invalidateRoomContents(qc, roomId);
}

export function useRoomState(roomId: string | undefined, type?: string) {
  return useQuery({
    queryKey: ["room-state", roomId, type || null],
    enabled: Boolean(roomId),
    queryFn: async () => {
      const result = await api.GET("/rooms/{room_id}/state", {
        params: { path: { room_id: roomId! }, query: { limit: 500, type: type || undefined } },
      });
      return unwrap(result);
    },
  });
}

/** One page of the timeline; `cursor` continues from an earlier page's `next_cursor`. */
export function useRoomMessages(
  roomId: string | undefined,
  options: { cursor?: string; dir?: "f" | "b"; limit?: number; enabled?: boolean } = {},
) {
  const { cursor, dir = "b", limit = 50, enabled = true } = options;
  return useQuery({
    queryKey: ["room-messages", roomId, cursor ?? null, dir, limit],
    enabled: Boolean(roomId) && enabled,
    queryFn: async () => {
      const result = await api.GET("/rooms/{room_id}/messages", {
        params: { path: { room_id: roomId! }, query: { cursor, dir, limit } },
      });
      return unwrap(result);
    },
  });
}

/** The timeline newest first, a page at a time: `fetchNextPage` loads older messages. */
export function useRoomTimeline(roomId: string | undefined, enabled = true, limit = 25) {
  return useInfiniteQuery({
    queryKey: ["room-messages", roomId, "timeline", limit],
    enabled: Boolean(roomId) && enabled,
    initialPageParam: undefined as string | undefined,
    queryFn: async ({ pageParam }) => {
      const result = await api.GET("/rooms/{room_id}/messages", {
        params: { path: { room_id: roomId! }, query: { cursor: pageParam, dir: "b", limit } },
      });
      return unwrap(result);
    },
    getNextPageParam: (last) => last.next_cursor ?? undefined,
  });
}

export function useRoomEvent(roomId: string | undefined, eventId: string | undefined) {
  return useQuery({
    queryKey: ["room-event", roomId, eventId],
    enabled: Boolean(roomId && eventId),
    queryFn: async () => {
      const result = await api.GET("/rooms/{room_id}/events/{event_id}", {
        params: { path: { room_id: roomId!, event_id: eventId! } },
      });
      return unwrap(result);
    },
  });
}

export function useEventContext(
  roomId: string | undefined,
  eventId: string | undefined,
  limit = 5,
) {
  return useQuery({
    queryKey: ["room-event-context", roomId, eventId, limit],
    enabled: Boolean(roomId && eventId),
    queryFn: async () => {
      const result = await api.GET("/rooms/{room_id}/events/{event_id}/context", {
        params: { path: { room_id: roomId!, event_id: eventId! }, query: { limit } },
      });
      return unwrap(result);
    },
  });
}

/** `rooms.events.at`: the event nearest `ts` (milliseconds), looking forwards or backwards. */
export function useEventAt() {
  return useMutation({
    mutationFn: async ({
      roomId,
      ts,
      dir = "f",
    }: {
      roomId: string;
      ts: number;
      dir?: "f" | "b";
    }) => {
      const result = await api.GET("/rooms/{room_id}/events/at", {
        params: { path: { room_id: roomId }, query: { ts, dir } },
      });
      return unwrap(result);
    },
  });
}

/** `events.get`: any event this server holds, by its id alone. */
export function useFindEvent() {
  return useMutation({
    mutationFn: async (eventId: string) => {
      const result = await api.GET("/events/{event_id}", {
        params: { path: { event_id: eventId } },
      });
      return unwrap(result);
    },
  });
}

export function useRoomAliases(roomId: string | undefined) {
  return useQuery({
    queryKey: ["room-aliases", roomId],
    enabled: Boolean(roomId),
    queryFn: async () => {
      const result = await api.GET("/rooms/{room_id}/aliases", {
        params: { path: { room_id: roomId! } },
      });
      return unwrap(result);
    },
  });
}

export function useAddRoomAlias() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ roomId, alias }: { roomId: string; alias: string }) => {
      const result = await api.POST("/rooms/{room_id}/aliases", {
        params: { path: { room_id: roomId }, header: { "Idempotency-Key": newIdempotencyKey() } },
        body: { alias },
      });
      return unwrap(result);
    },
    onSuccess: (_data, { roomId }) => invalidateRoomContents(qc, roomId),
  });
}

export function useRemoveRoomAlias() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ roomId, alias }: { roomId: string; alias: string }) => {
      const result = await api.DELETE("/rooms/{room_id}/aliases/{alias}", {
        params: { path: { room_id: roomId, alias } },
      });
      unwrap(result);
      return null;
    },
    onSuccess: (_data, { roomId }) => invalidateRoomContents(qc, roomId),
  });
}

export function useRoomHierarchy(roomId: string | undefined) {
  return useQuery({
    queryKey: ["room-hierarchy", roomId],
    enabled: Boolean(roomId),
    queryFn: async () => {
      const result = await api.GET("/rooms/{room_id}/hierarchy", {
        params: { path: { room_id: roomId! }, query: { limit: 200 } },
      });
      return unwrap(result);
    },
  });
}

export function useJoinRoom() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ roomId, userId }: { roomId: string; userId: string }) => {
      const result = await api.POST("/rooms/{room_id}/join", {
        params: { path: { room_id: roomId }, header: { "Idempotency-Key": newIdempotencyKey() } },
        body: { user_id: userId },
      });
      return unwrap(result);
    },
    onSuccess: (_data, { roomId }) => invalidateRoomContents(qc, roomId),
  });
}

export function useForwardExtremities(roomId: string | undefined, enabled = true) {
  return useQuery({
    queryKey: ["room-extremities", roomId],
    enabled: Boolean(roomId) && enabled,
    queryFn: async () => {
      const result = await api.GET("/rooms/{room_id}/forward-extremities", {
        params: { path: { room_id: roomId! } },
      });
      return unwrap(result);
    },
  });
}

export function usePruneForwardExtremities() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (roomId: string) => {
      const result = await api.DELETE("/rooms/{room_id}/forward-extremities", {
        params: { path: { room_id: roomId } },
      });
      return unwrap(result);
    },
    onSuccess: (_data, roomId) => invalidateRoomContents(qc, roomId),
  });
}

export function useRoomMedia(roomId: string | undefined) {
  return useQuery({
    queryKey: ["room-media", roomId],
    enabled: Boolean(roomId),
    queryFn: async () => {
      const result = await api.GET("/rooms/{room_id}/media", {
        params: { path: { room_id: roomId! }, query: { limit: 200 } },
      });
      return unwrap(result);
    },
  });
}

/** `rooms.media.quarantine`: answered `202` with the task that does it. */
export function useQuarantineRoomMedia() {
  return useMutation({
    mutationFn: async (roomId: string) => {
      const result = await api.POST("/rooms/{room_id}/media/quarantine", {
        params: { path: { room_id: roomId }, header: { "Idempotency-Key": newIdempotencyKey() } },
      });
      return unwrap(result);
    },
  });
}

export interface PurgeHistoryRequest {
  roomId: string;
  /** RFC 3339: messages sent before this are purged. */
  before: string;
  deleteLocalEvents: boolean;
}

/** `rooms.purge_history`: answered `202` with the task that does it. */
export function usePurgeRoomHistory() {
  return useMutation({
    mutationFn: async ({ roomId, before, deleteLocalEvents }: PurgeHistoryRequest) => {
      const result = await api.POST("/rooms/{room_id}/purge-history", {
        params: { path: { room_id: roomId }, header: { "Idempotency-Key": newIdempotencyKey() } },
        body: { before, delete_local_events: deleteLocalEvents },
      });
      return unwrap(result);
    },
  });
}

export interface DeleteRoomRequest {
  roomId: string;
  block: boolean;
  purge: boolean;
  /** Posted in the new room; only sent with `newRoom`. */
  message?: string;
  newRoom?: { name?: string; creator: string };
}

/** `rooms.delete`: answered `202` with the task that does it. */
export function useDeleteRoom() {
  return useMutation({
    mutationFn: async ({ roomId, block, purge, message, newRoom }: DeleteRoomRequest) => {
      const result = await api.POST("/rooms/{room_id}/delete", {
        params: { path: { room_id: roomId }, header: { "Idempotency-Key": newIdempotencyKey() } },
        body: {
          block,
          purge,
          ...(newRoom ? { new_room: newRoom, message: message || undefined } : {}),
        },
      });
      return unwrap(result);
    },
  });
}
