import { isRangeId, type RangeId } from "@/api/statistics";
import { sortIn } from "@/lib/sort-param";

export { fromSortState, toSortState } from "@/lib/sort-param";

/**
 * The Statistics page's URL state: the chart range, and each table's sort and page cursor. The
 * sorts are the operations' own `sort` values (`-joined_members_count`, `media_count`).
 */
export interface StatisticsSearch {
  range?: RangeId;
  rooms_sort?: string;
  rooms_cursor?: string;
  media_sort?: string;
  media_cursor?: string;
}

export const ROOM_SORT_FIELDS = ["joined_members_count", "state_events_count"] as const;
export const MEDIA_SORT_FIELDS = ["media_bytes", "media_count"] as const;

function text(value: unknown): string | undefined {
  return typeof value === "string" && value ? value : undefined;
}

export function validateStatisticsSearch(search: Record<string, unknown>): StatisticsSearch {
  return {
    range: isRangeId(search.range) ? search.range : undefined,
    rooms_sort: sortIn(ROOM_SORT_FIELDS, search.rooms_sort),
    rooms_cursor: text(search.rooms_cursor),
    media_sort: sortIn(MEDIA_SORT_FIELDS, search.media_sort),
    media_cursor: text(search.media_cursor),
  };
}
