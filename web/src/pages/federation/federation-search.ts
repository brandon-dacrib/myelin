import { DESTINATION_SORT_FIELDS } from "@/lib/federation";
import { sortIn } from "@/lib/sort-param";

/**
 * Which destinations the Federation page lists: every one, only those whose requests are
 * failing (`failing=true`), or only the rest (`failing=false`: healthy and backing off).
 */
export type DestinationShow = "failing" | "not-failing";

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
    show: search.show === "failing" || search.show === "not-failing" ? search.show : undefined,
    sort: sortIn(DESTINATION_SORT_FIELDS, search.sort),
    cursor: typeof search.cursor === "string" && search.cursor ? search.cursor : undefined,
  };
}

/** The `failing` query parameter for a filter: absent for every server. */
export function failingParam(show: DestinationShow | undefined): boolean | undefined {
  if (show === "failing") return true;
  if (show === "not-failing") return false;
  return undefined;
}
