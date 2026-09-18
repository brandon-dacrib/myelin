/** Rooms (flows.md flow 3), against the real `/rooms` resources in `crates/hs-admin/openapi/openapi.yaml`. */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import type { components } from "./schema";

export type Room = components["schemas"]["Room"];
export type RoomMember = components["schemas"]["RoomMember"];

export interface RoomListFilters {
  q?: string;
  cursor?: string;
  limit?: number;
  public?: boolean;
  blocked?: boolean;
  encrypted?: boolean;
}

export function useRooms(filters: RoomListFilters) {
  return useQuery({
    queryKey: ["rooms", filters],
    queryFn: async () => {
      const { data, error } = await api.GET("/rooms", { params: { query: filters } });
      if (error) throw error;
      return data;
    },
    refetchInterval: 30_000,
  });
}

export function useRoom(roomId: string | undefined) {
  return useQuery({
    queryKey: ["room", roomId],
    enabled: Boolean(roomId),
    queryFn: async () => {
      const { data, error } = await api.GET("/rooms/{room_id}", {
        params: { path: { room_id: roomId! } },
      });
      if (error) throw error;
      return data;
    },
  });
}

export function useRoomMembers(roomId: string | undefined) {
  return useQuery({
    queryKey: ["room-members", roomId],
    enabled: Boolean(roomId),
    queryFn: async () => {
      const { data, error } = await api.GET("/rooms/{room_id}/members", {
        params: { path: { room_id: roomId! }, query: { limit: 50 } },
      });
      if (error) throw error;
      return data;
    },
  });
}

function invalidateRoom(qc: ReturnType<typeof useQueryClient>, roomId: string) {
  qc.invalidateQueries({ queryKey: ["rooms"] });
  qc.invalidateQueries({ queryKey: ["room", roomId] });
}

export function useBlockRoom() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ roomId, reason }: { roomId: string; reason?: string }) => {
      const { data, error } = await api.POST("/rooms/{room_id}/block", {
        params: { path: { room_id: roomId }, header: { "Idempotency-Key": newIdempotencyKey() } },
        body: reason ? { reason } : undefined,
      });
      if (error) throw error;
      return data;
    },
    onSuccess: (_data, { roomId }) => invalidateRoom(qc, roomId),
  });
}

export function useUnblockRoom() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (roomId: string) => {
      const { data, error } = await api.POST("/rooms/{room_id}/unblock", {
        params: { path: { room_id: roomId }, header: { "Idempotency-Key": newIdempotencyKey() } },
      });
      if (error) throw error;
      return data;
    },
    onSuccess: (_data, roomId) => invalidateRoom(qc, roomId),
  });
}

export function useMakeRoomAdmin() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (roomId: string) => {
      const { data, error } = await api.POST("/rooms/{room_id}/make-admin", {
        params: { path: { room_id: roomId }, header: { "Idempotency-Key": newIdempotencyKey() } },
      });
      if (error) throw error;
      return data;
    },
    onSuccess: (_data, roomId) => invalidateRoom(qc, roomId),
  });
}
