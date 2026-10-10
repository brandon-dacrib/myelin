import { DESTINATION_SORT_FIELDS } from "@/lib/federation";
import { sortIn } from "@/lib/sort-param";

/**
 * Which destinations the Federation page lists: every one, only those whose requests are
 * failing (`failing=true`), only the rest (`failing=false`: healthy and backing off), or only
 * those this server shares no room with (`shares_room=false`: the ones a prune may forget).
 */
export type DestinationShow = "failing" | "not-failing" | "no-shared-room";

const SHOWS: readonly DestinationShow[] = ["failing", "not-failing", "no-shared-room"];

/**
 * The Federation page's URL state: the filter, the sort (`GET /federation/destinations`'s own
 * `sort` value, `-` in front for descending) and the page cursor.
 */
export interface FederationSearch {
  show?: DestinationShow;
  sort?: string;
  cursor?: string;
}

export function validateFederationSearch(search: Record<string, unknown>): FederationSearch {
  return {
    show: SHOWS.find((s) => s === search.show),
    sort: sortIn(DESTINATION_SORT_FIELDS, search.sort),
    cursor: typeof search.cursor === "string" && search.cursor ? search.cursor : undefined,
  };
}

/** The `failing` query parameter for a filter: absent unless the filter is about failing. */
export function failingParam(show: DestinationShow | undefined): boolean | undefined {
  if (show === "failing") return true;
  if (show === "not-failing") return false;
  return undefined;
}

/** The `shares_room` query parameter for a filter: `false` for "no shared room", else absent. */
export function sharesRoomParam(show: DestinationShow | undefined): boolean | undefined {
  return show === "no-shared-room" ? false : undefined;
}
