/** The room page's search parameters: which tab is open, and which event the timeline shows. */

export const ROOM_TABS = [
  "overview",
  "state",
  "timeline",
  "aliases",
  "hierarchy",
  "media",
  "extremities",
] as const;
export type RoomTab = (typeof ROOM_TABS)[number];

export interface RoomSearch {
  tab?: RoomTab;
  event?: string;
}

export function validateRoomSearch(search: Record<string, unknown>): RoomSearch {
  const tab = ROOM_TABS.find((t) => t === search.tab);
  return {
    tab: tab && tab !== "overview" ? tab : undefined,
    event: typeof search.event === "string" && search.event ? search.event : undefined,
  };
}
