/**
 * `/media`'s search parameters: the list's filters live in the URL, so a view can be linked and
 * survives a reload. Its own module (not `MediaPage.tsx`) so the route table can validate them
 * without pulling the page out of its lazily loaded chunk.
 */
import { MEDIA_SORTS, type MediaSort } from "@/api/media";

export interface MediaSearch {
  q?: string;
  origin?: "local" | "remote";
  status?: "quarantined" | "protected";
  sort?: MediaSort;
  cursor?: string;
}

function pick<T extends string>(value: unknown, allowed: readonly T[]): T | undefined {
  return allowed.includes(value as T) ? (value as T) : undefined;
}

export function validateMediaSearch(search: Record<string, unknown>): MediaSearch {
  return {
    q: typeof search.q === "string" && search.q !== "" ? search.q : undefined,
    origin: pick(search.origin, ["local", "remote"] as const),
    status: pick(search.status, ["quarantined", "protected"] as const),
    sort: pick(search.sort, MEDIA_SORTS),
    cursor: typeof search.cursor === "string" ? search.cursor : undefined,
  };
}
