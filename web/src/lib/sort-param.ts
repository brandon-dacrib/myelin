import type { SortState } from "@/components/ui/table/DataTable";

/**
 * The admin API's `sort` parameter (`field`, or `-field` for descending; RFC 0004 section 5)
 * to and from the table's sort state.
 */
export function toSortState(sort: string): SortState {
  return sort.startsWith("-")
    ? { key: sort.slice(1), direction: "desc" }
    : { key: sort, direction: "asc" };
}

export function fromSortState(state: SortState): string {
  return state.direction === "desc" ? `-${state.key}` : state.key;
}

/** `value` when it names one of `fields`, with or without a leading `-`; otherwise nothing. */
export function sortIn(fields: readonly string[], value: unknown): string | undefined {
  if (typeof value !== "string") return undefined;
  return fields.includes(value.replace(/^-/, "")) ? value : undefined;
}
