import type { ShardKind } from "@/api/cluster";
import { isShardKind } from "@/lib/cluster";

/** The shard view: the map of every shard coloured by owner, or the paged table. */
export type ShardView = "map" | "table";

/** The Cluster page's shard filters as they live in the URL. */
export interface ClusterSearch {
  kind?: ShardKind;
  view?: ShardView;
  /** The table view's page (`GET /cluster/shards`'s cursor). */
  cursor?: string;
}

export function validateClusterSearch(search: Record<string, unknown>): ClusterSearch {
  return {
    kind: isShardKind(search.kind) ? search.kind : undefined,
    view: search.view === "table" ? "table" : undefined,
    cursor: typeof search.cursor === "string" && search.cursor ? search.cursor : undefined,
  };
}
