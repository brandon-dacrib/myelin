import { Link } from "@tanstack/react-router";
import type { UseQueryResult } from "@tanstack/react-query";
import type { RoomHierarchyNode } from "@/api/room-contents";
import { Badge } from "@/components/ui/badge/Badge";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { QueryProblemState } from "@/components/QueryProblemState";

/** A space's rooms, as deep as the server walked, indented by depth. */
export function RoomHierarchyTab({
  hierarchy,
}: {
  hierarchy: UseQueryResult<{ items: RoomHierarchyNode[] }>;
}) {
  return (
    <section aria-labelledby="room-hierarchy-heading">
      <h2 id="room-hierarchy-heading" className="text-md font-medium text-text">
        Space hierarchy
      </h2>
      {hierarchy.isLoading ? (
        <SkeletonText lines={3} />
      ) : hierarchy.isError ? (
        <QueryProblemState
          error={hierarchy.error}
          resource="this space's rooms"
          onRetry={() => hierarchy.refetch()}
        />
      ) : (
        <ul aria-label="Hierarchy" className="mt-3 flex flex-col gap-1">
          {hierarchy.data?.items.map((node) => (
            <li
              key={`${node.depth}-${node.room_id}`}
              className="flex flex-wrap items-center gap-2 rounded-sm border border-border px-3 py-2"
              style={{ marginLeft: `${node.depth * 1.5}rem` }}
            >
              {node.known ? (
                <Link
                  to="/rooms/$roomId"
                  params={{ roomId: node.room_id }}
                  className="font-medium text-text hover:text-accent hover:underline"
                >
                  {node.name ?? node.canonical_alias ?? node.room_id}
                </Link>
              ) : (
                <span className="font-identifier text-text-muted">{node.room_id}</span>
              )}
              {node.room_type === "m.space" && (
                <Badge status="info" hideIcon>
                  Space
                </Badge>
              )}
              {!node.known && (
                <Badge status="muted" hideIcon>
                  Not on this server
                </Badge>
              )}
              {node.joined_members_count != null && (
                <span className="text-xs text-text-muted">
                  {node.joined_members_count.toLocaleString()} joined
                </span>
              )}
              {node.depth === 0 && (
                <span className="text-xs text-text-muted">
                  {node.children.length} {node.children.length === 1 ? "child" : "children"}
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
