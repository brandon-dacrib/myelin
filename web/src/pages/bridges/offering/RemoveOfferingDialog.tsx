import { useState } from "react";
import { useNavigate } from "@tanstack/react-router";
import { useDeleteBridgeOffering, type BridgeOffering } from "@/api/bridges";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { toast } from "@/components/ui/toast/toast-store";

/**
 * Stops offering a bridge. The server refuses (409) while people still have one; the dialog
 * then says what removing them all means and asks for the bridge's name before it does
 * (`?remove_instances=true`): everyone's sign-ins go with them.
 */
export function RemoveOfferingDialog({
  offering,
  open,
  onOpenChange,
}: {
  offering: BridgeOffering;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const remove = useDeleteBridgeOffering();
  const navigate = useNavigate();
  const [blocked, setBlocked] = useState<string | null>(null);
  const [typed, setTyped] = useState("");
  const [error, setError] = useState<string | null>(null);
  const name = offering.name ?? offering.type;

  function handleOpenChange(next: boolean) {
    if (!next) {
      setBlocked(null);
      setTyped("");
      setError(null);
    }
    onOpenChange(next);
  }

  async function attempt(removeInstances: boolean) {
    setError(null);
    try {
      await remove.mutateAsync({ type: offering.type, removeInstances });
      toast({ title: `${name} is no longer offered` });
      handleOpenChange(false);
      navigate({ to: "/bridges" });
    } catch (err) {
      if (err instanceof ApiProblemError && err.problem.status === 409 && !removeInstances) {
        setBlocked(err.problem.detail ?? "People still have this bridge.");
        return;
      }
      setError(
        err instanceof ApiProblemError
          ? (err.problem.detail ?? err.problem.title ?? "The server refused.")
          : "Couldn’t reach the server.",
      );
    }
  }

  if (blocked) {
    return (
      <Dialog open={open} onOpenChange={handleOpenChange}>
        <DialogContent
          title={`Remove everyone's ${name} bridge?`}
          description={
            <>
              {blocked} Removing them deletes every one of these bridges: their pods, their volumes,
              and everyone&apos;s sign-ins. People have to link {name} again from scratch if it is
              offered again. This cannot be undone.
            </>
          }
          footer={
            <>
              <Button variant="secondary" onClick={() => handleOpenChange(false)}>
                Keep offering it
              </Button>
              <Button
                variant="danger"
                disabled={typed.trim() !== name || remove.isPending}
                onClick={() => attempt(true)}
              >
                {remove.isPending ? "Removing..." : "Remove all and stop offering"}
              </Button>
            </>
          }
        >
          <Field label={`Type ${name} to confirm`} error={error ?? undefined}>
            {(f) => (
              <Input
                {...f}
                value={typed}
                autoComplete="off"
                onChange={(e) => setTyped(e.target.value)}
              />
            )}
          </Field>
        </DialogContent>
      </Dialog>
    );
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        title={`Stop offering ${name}?`}
        description={
          offering.front_door
            ? `${offering.front_door} stops answering, and nobody new can get a ${name} bridge.`
            : `Nobody can use ${name} through this server any more.`
        }
        footer={
          <>
            <Button variant="secondary" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button variant="danger" disabled={remove.isPending} onClick={() => attempt(false)}>
              {remove.isPending ? "Stopping..." : "Stop offering"}
            </Button>
          </>
        }
      >
        {error && (
          <p role="alert" className="text-sm text-danger">
            {error}
          </p>
        )}
      </DialogContent>
    </Dialog>
  );
}
