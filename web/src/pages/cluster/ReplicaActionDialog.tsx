import { useDrainReplica, useUndrainReplica, type Replica } from "@/api/cluster";
import { MutationError } from "@/components/MutationError";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { toast } from "@/components/ui/toast/toast-store";
import { joinWithAnd, plural } from "@/lib/cluster";

export type ReplicaAction = "drain" | "undrain";

export interface PendingReplicaAction {
  action: ReplicaAction;
  replica: Replica;
}

/**
 * The confirmation for a drain or an undrain: says what will happen to the replica and its
 * shards before it does, and if the server refuses, says why in the server's own words (a 409
 * when nothing could take the shards) without closing, so the operator reads it where they
 * asked.
 */
export function ReplicaActionDialog({
  pending,
  replicas,
  onClose,
}: {
  pending: PendingReplicaAction | null;
  replicas: Replica[];
  onClose: () => void;
}) {
  const drain = useDrainReplica();
  const undrain = useUndrainReplica();
  const mutation = pending?.action === "undrain" ? undrain : drain;

  function close() {
    drain.reset();
    undrain.reset();
    onClose();
  }

  return (
    <Dialog open={pending !== null} onOpenChange={(open) => !open && close()}>
      {pending && (
        <DialogContent
          title={`${pending.action === "drain" ? "Drain" : "Undrain"} ${pending.replica.id}?`}
          description={
            pending.action === "drain"
              ? "Draining hands every shard this replica owns to the other active replicas. It keeps running and serving requests, forwarding each one to the shard's new owner."
              : "The replica becomes active again and takes back its share of the shards from the others."
          }
          footer={
            <>
              <DialogClose asChild>
                <Button variant="secondary">Cancel</Button>
              </DialogClose>
              <Button
                disabled={mutation.isPending}
                onClick={() =>
                  mutation.mutate(pending.replica.id ?? "", {
                    onSuccess: (replica) => {
                      toast({
                        title:
                          pending.action === "drain"
                            ? replica.status === "drained"
                              ? `${replica.id} is drained`
                              : `${replica.id} is draining`
                            : `${replica.id} is active again`,
                        description:
                          pending.action === "drain" && replica.status === "draining"
                            ? "Its shards are moving to the other replicas; this page follows along."
                            : undefined,
                      });
                      close();
                    },
                  })
                }
              >
                {pending.action === "drain" ? "Drain replica" : "Undrain replica"}
              </Button>
            </>
          }
        >
          <div className="flex flex-col gap-3 text-sm text-text">
            {pending.action === "drain" ? (
              <DrainConsequences replica={pending.replica} replicas={replicas} />
            ) : (
              <UndrainConsequences replica={pending.replica} />
            )}
            {mutation.isError && (
              <MutationError
                error={mutation.error}
                action={pending.action === "drain" ? "drain it" : "undrain it"}
              />
            )}
          </div>
        </DialogContent>
      )}
    </Dialog>
  );
}

function DrainConsequences({ replica, replicas }: { replica: Replica; replicas: Replica[] }) {
  const takers = replicas
    .filter((r) => r.id !== replica.id && r.status === "active")
    .map((r) => r.id ?? "");
  const owned = replica.shard_count ?? 0;
  return (
    <ul className="list-disc space-y-2 pl-5">
      <li>
        It owns {plural(owned, "shard", "shards")}
        {takers.length > 0
          ? `; ${owned === 1 ? "it moves" : "they move"} to ${joinWithAnd(takers)}.`
          : ". No other replica is active to take them, so the server is likely to refuse."}
      </li>
      <li>
        The request is kept in the database, so it survives a restart: a drained replica comes back
        drained until you undrain it.
      </li>
      <li>Undrain gives it its share of the shards back.</li>
      {replica.this_replica && (
        <li>
          This is the replica answering this page. It keeps answering; only the work its shards
          stand for moves.
        </li>
      )}
    </ul>
  );
}

function UndrainConsequences({ replica }: { replica: Replica }) {
  return (
    <ul className="list-disc space-y-2 pl-5">
      {replica.status === "draining" ? (
        <li>
          The drain still under way stops, and its task is cancelled. Shards already handed off come
          back as the replicas rebalance.
        </li>
      ) : replica.role !== "single-node" && !replica.last_heartbeat_at ? (
        <li>
          It is not running. Undraining withdraws the drain request, so it takes its share of the
          shards back the next time it starts.
        </li>
      ) : (
        <li>Shards move back to it from the other replicas as they rebalance.</li>
      )}
      <li>You can drain it again at any time.</li>
    </ul>
  );
}
