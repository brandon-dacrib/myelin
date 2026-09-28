import { useForwardExtremities, usePruneForwardExtremities } from "@/api/room-contents";
import { Button } from "@/components/ui/button/Button";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { QueryProblemState } from "@/components/QueryProblemState";
import { MutationError } from "@/components/MutationError";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";

/**
 * The room's forward extremities: the newest events nothing yet follows. One is healthy; more
 * means the room's history forked, and pruning keeps only the newest.
 */
export function RoomExtremitiesTab({ roomId }: { roomId: string }) {
  const canRead = hasScope("admin:read");
  const extremities = useForwardExtremities(roomId, canRead);
  const prune = usePruneForwardExtremities();
  const canWrite = hasScope("admin:write");

  if (!canRead) return <ForbiddenState scope="admin:read" compact />;
  const items = extremities.data ?? [];
  return (
    <section aria-labelledby="room-extremities-heading">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 id="room-extremities-heading" className="text-md font-medium text-text">
          Forward extremities
        </h2>
        {items.length > 1 && (
          <Button
            variant="secondary"
            disabled={!canWrite || prune.isPending}
            title={canWrite ? undefined : "Needs admin:write"}
            onClick={() =>
              prune.mutate(roomId, {
                onSuccess: (result) =>
                  toast({
                    title: `Pruned ${result.deleted.length} ${result.deleted.length === 1 ? "extremity" : "extremities"}`,
                    description: `${result.remaining.length} left.`,
                  }),
              })
            }
          >
            Prune to one
          </Button>
        )}
      </div>
      <p className="mt-1 text-sm text-text-muted">
        {items.length <= 1
          ? "One extremity: the room's history is a single line."
          : `${items.length} extremities: the room's history has forked. A later event normally merges them; pruning keeps only the newest.`}
      </p>
      {prune.isError && (
        <MutationError className="mt-2" error={prune.error} action="prune the extremities" />
      )}
      {extremities.isLoading ? (
        <SkeletonText lines={2} />
      ) : extremities.isError ? (
        <QueryProblemState
          error={extremities.error}
          resource="this room's forward extremities"
          onRetry={() => extremities.refetch()}
        />
      ) : (
        <table className="mt-3 w-full text-left text-sm" aria-label="Forward extremities">
          <thead className="text-xs text-text-muted">
            <tr>
              <th className="py-2 pr-3 font-medium">Event</th>
              <th className="py-2 pr-3 font-medium">Type</th>
              <th className="py-2 pr-3 font-medium">Sender</th>
              <th className="py-2 pr-3 text-right font-medium">Depth</th>
              <th className="py-2 font-medium">Sent</th>
            </tr>
          </thead>
          <tbody className="divide-y divide-border">
            {items.map((e) => (
              <tr key={e.event_id}>
                <td className="py-2 pr-3 font-identifier text-text">{e.event_id}</td>
                <td className="py-2 pr-3 font-identifier text-text-muted">{e.type}</td>
                <td className="py-2 pr-3 font-identifier text-text-muted">{e.sender}</td>
                <td className="py-2 pr-3 text-right tabular-nums">{e.depth}</td>
                <td className="py-2 text-text-muted">
                  {new Date(e.origin_server_ts).toLocaleString()}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}
