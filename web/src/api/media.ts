/**
 * Media (the admin API's nine `media.*` operations in `crates/hs-admin/openapi/openapi.yaml`),
 * plus the one Matrix call the Media page makes itself: an authenticated thumbnail, fetched with
 * the operator's own token, so a preview shows what a client would see.
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, apiBaseUrl, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import type { components } from "./schema";
import { getAccessToken } from "@/lib/auth";

export type MediaItem = components["schemas"]["MediaItem"];
export type Task = components["schemas"]["Task"];

/** The sort orders the list offers; `-` is descending (the API's own convention). */
export const MEDIA_SORTS = [
  "-created_at",
  "created_at",
  "-size_bytes",
  "-last_accessed_at",
  "last_accessed_at",
] as const;
export type MediaSort = (typeof MEDIA_SORTS)[number];

export interface MediaListFilters {
  q?: string;
  origin?: "local" | "remote";
  quarantined?: boolean;
  protected?: boolean;
  uploader?: string;
  sort?: MediaSort;
  cursor?: string;
  limit?: number;
}

export function mediaKey(item: Pick<MediaItem, "server_name" | "media_id">): string {
  return `mxc://${item.server_name}/${item.media_id}`;
}

/** The name an operator knows an item by: its filename, or failing that its id. */
export function mediaName(item: Pick<MediaItem, "upload_name" | "media_id">): string {
  return item.upload_name ?? item.media_id;
}

export function useMediaList(filters: MediaListFilters) {
  return useQuery({
    queryKey: ["media", filters],
    queryFn: async () => {
      const result = await api.GET("/media", {
        params: { query: { ...filters, include_total: true } },
      });
      return unwrap(result);
    },
    refetchInterval: 30_000,
  });
}

export function useMediaItem(item: Pick<MediaItem, "server_name" | "media_id"> | undefined) {
  return useQuery({
    queryKey: ["media-item", item?.server_name, item?.media_id],
    enabled: Boolean(item),
    queryFn: async () => {
      const result = await api.GET("/media/{server_name}/{media_id}", {
        params: { path: { server_name: item!.server_name, media_id: item!.media_id } },
      });
      return unwrap(result);
    },
  });
}

function invalidateMedia(qc: ReturnType<typeof useQueryClient>) {
  qc.invalidateQueries({ queryKey: ["media"] });
  qc.invalidateQueries({ queryKey: ["media-item"] });
}

export type MediaFlagAction = "quarantine" | "unquarantine" | "protect" | "unprotect";

/** Quarantine, lift a quarantine, protect or unprotect one item. */
export function useMediaFlagAction() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async ({ item, action }: { item: MediaItem; action: MediaFlagAction }) => {
      const params = {
        path: { server_name: item.server_name, media_id: item.media_id },
        header: { "Idempotency-Key": newIdempotencyKey() },
      };
      const path = {
        quarantine: "/media/{server_name}/{media_id}/quarantine",
        unquarantine: "/media/{server_name}/{media_id}/unquarantine",
        protect: "/media/{server_name}/{media_id}/protect",
        unprotect: "/media/{server_name}/{media_id}/unprotect",
      } as const;
      return unwrap(await api.POST(path[action], { params }));
    },
    onSuccess: () => invalidateMedia(qc),
  });
}

export function useDeleteMedia() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (item: MediaItem) => {
      const result = await api.DELETE("/media/{server_name}/{media_id}", {
        params: { path: { server_name: item.server_name, media_id: item.media_id } },
      });
      unwrap(result);
    },
    onSuccess: () => invalidateMedia(qc),
  });
}

/** What a bulk deletion's finished Task reports in its `result`. */
export interface PurgeResult {
  deleted_count: number;
  deleted_bytes: number;
  skipped_protected: number;
  skipped_quarantined: number;
  failed: string[];
}

export function purgeResult(task: Task): PurgeResult {
  const r = (task.result ?? {}) as Partial<PurgeResult>;
  return {
    deleted_count: r.deleted_count ?? 0,
    deleted_bytes: r.deleted_bytes ?? 0,
    skipped_protected: r.skipped_protected ?? 0,
    skipped_quarantined: r.skipped_quarantined ?? 0,
    failed: r.failed ?? [],
  };
}

/** `POST /media/delete`: this server's uploads unused since `before`. */
export function useBulkDeleteMedia() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (body: { before: string; min_size_bytes?: number }) => {
      const result = await api.POST("/media/delete", {
        params: { header: { "Idempotency-Key": newIdempotencyKey() } },
        body,
      });
      return unwrap(result);
    },
    onSuccess: () => invalidateMedia(qc),
  });
}

/** `POST /media/purge-remote-cache`: cached copies of other servers' media unused since `before`. */
export function usePurgeRemoteMediaCache() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (body: { before: string; server_name?: string }) => {
      const result = await api.POST("/media/purge-remote-cache", {
        params: { header: { "Idempotency-Key": newIdempotencyKey() } },
        body,
      });
      return unwrap(result);
    },
    onSuccess: () => invalidateMedia(qc),
  });
}

/**
 * The authenticated-media thumbnail URL for `item` (`GET /_matrix/client/v1/media/thumbnail`),
 * on the same server the admin API is on.
 */
export function thumbnailUrl(
  item: Pick<MediaItem, "server_name" | "media_id">,
  size: { width: number; height: number; method: "crop" | "scale" },
): string {
  const apiRoot = new URL(apiBaseUrl(), window.location.origin);
  const root = apiRoot.href.replace(/\/api\/v1\/?$/, "");
  const query = new URLSearchParams({
    width: String(size.width),
    height: String(size.height),
    method: size.method,
    animated: "false",
  });
  return `${root}/_matrix/client/v1/media/thumbnail/${encodeURIComponent(item.server_name)}/${encodeURIComponent(item.media_id)}?${query}`;
}

/** Whether a preview is worth asking for: an image a client could be shown. */
export function hasPreview(item: MediaItem): boolean {
  return Boolean(item.content_type?.startsWith("image/")) && !item.quarantined;
}

/** Fetches a thumbnail with the operator's token (an `<img src>` cannot send one). */
export async function fetchThumbnail(url: string, signal: AbortSignal): Promise<Blob> {
  const token = getAccessToken();
  const response = await fetch(url, {
    signal,
    headers: token ? { Authorization: `Bearer ${token}` } : undefined,
  });
  if (!response.ok) throw new Error(`thumbnail: ${response.status}`);
  return response.blob();
}
