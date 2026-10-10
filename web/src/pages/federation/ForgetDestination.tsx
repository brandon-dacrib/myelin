/**
 * Forgetting a federation destination (`DELETE /federation/destinations/{server_name}`,
 * decision 0042): every server this one ever sent to is remembered with its queue and retry
 * state; once no room brings the two together it is only state, and an operator may drop it.
 * The dialog says what goes (queued events, backoff, catch-up mark, cached keys), warns when
 * the server still shares a room and asks for the force in that case, and shows the server's
 * refusal when it answers `409` on its own account.
 */
import { useState, type ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Switch } from "@/components/ui/switch/Switch";
import { MutationError } from "@/components/MutationError";
import { ApiProblemError } from "@/api/problem";
import {
  useForgetDestination,
  type Destination,
  type DestinationForgotten,
} from "@/api/federation";
import { forgetConsequence, things } from "@/lib/federation";

export function ForgetDestinationDialog({
  destination,
  open,
  onOpenChange,
  onForgotten,
}: {
  destination: Destination;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Called with the server's answer once it has forgotten; the dialog has closed by then. */
  onForgotten?: (result: DestinationForgotten) => void;
}) {
  const forget = useForgetDestination();
  const [force, setForce] = useState(false);
  const name = destination.server_name ?? "";
  const shared = destination.shared_rooms_count ?? null;
  const refusedForSharing =
    forget.error instanceof ApiProblemError && forget.error.problem.status === 409;
  // The force is offered when the list says rooms are shared, or when the server said so.
  const offerForce = (shared != null && shared > 0) || refusedForSharing;

  function handleOpenChange(next: boolean) {
    if (!next) {
      setForce(false);
      forget.reset();
    }
    onOpenChange(next);
  }

  function confirm() {
    forget.mutate(
      { serverName: name, force: offerForce && force },
      {
        onSuccess: (result) => {
          handleOpenChange(false);
          onForgotten?.(result);
        },
      },
    );
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        title={
          <>
            Forget <span className="font-identifier">{name}</span>?
          </>
        }
        description={forgetConsequence(destination)}
      >
        <div className="flex flex-col gap-4 text-sm">
          {shared != null && shared > 0 ? (
            <SharedRoomsWarning name={name}>
              This server still shares {things(shared, "room")} with it: your users are in rooms its
              users are in, so it will be written to again at once and anything queued for it now is
              theirs. Forgetting it is for a server that is gone for good; otherwise leave it, or
              reset its backoff.
            </SharedRoomsWarning>
          ) : shared === 0 ? (
            <p className="text-text-muted">
              No room is shared with it, so nothing will be sent to it until one is: forgetting it
              costs nothing. The hourly sweep would forget it in time; this does it now.
            </p>
          ) : refusedForSharing ? (
            <SharedRoomsWarning name={name}>
              The server refused because it still shares a room with it (its answer is below).
              Forgetting it anyway drops what is queued for it, which your users&apos; rooms need.
            </SharedRoomsWarning>
          ) : (
            <p className="text-text-muted">
              This server cannot say whether it shares a room with it right now; it will refuse if
              it does, and you can then choose to forget it anyway.
            </p>
          )}
          {offerForce && (
            <div className="flex items-start justify-between gap-4 rounded-sm border border-border p-3">
              <div>
                <label htmlFor="forget-force" className="font-medium text-text">
                  Forget it anyway
                </label>
                <p className="mt-0.5 text-xs text-text-muted">
                  Drops its queue even though a room is shared (<code>force=true</code>). The audit
                  log records that you did.
                </p>
              </div>
              <Switch id="forget-force" checked={force} onCheckedChange={setForce} />
            </div>
          )}
          {forget.error != null && <MutationError error={forget.error} action={`forget ${name}`} />}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="secondary" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button
              type="button"
              variant="danger"
              disabled={forget.isPending || (offerForce && !force)}
              title={offerForce && !force ? "Turn on Forget it anyway first" : undefined}
              onClick={confirm}
            >
              {forget.isPending ? "Forgetting…" : "Forget"}
            </Button>
          </div>
        </div>
      </DialogContent>
    </Dialog>
  );
}

function SharedRoomsWarning({ name, children }: { name: string; children: ReactNode }) {
  return (
    <div
      role="note"
      className="rounded-sm border border-warning-border bg-warning-bg p-3 text-text"
    >
      <p>{children}</p>
      <p className="mt-2 text-xs text-text-muted">
        Which rooms:{" "}
        <Link
          to="/federation/$serverName"
          params={{ serverName: name }}
          className="text-accent underline hover:no-underline"
        >
          open the server
        </Link>{" "}
        and see its shared rooms.
      </p>
    </div>
  );
}
