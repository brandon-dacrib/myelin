import { useState, type FormEvent } from "react";
import {
  useBridgeInstances,
  usePutBridgeInstance,
  type BridgeInstance,
  type BridgeOffering,
  type BridgeType,
} from "@/api/bridges";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { toast } from "@/components/ui/toast/toast-store";
import { looksLikeUserId } from "@/lib/bridge-offerings";
import { InstanceNextSteps } from "./InstanceNextSteps";

/**
 * Sets up a bridge for someone, exactly as their message to the front door would
 * (`PUT .../instances/{user_id}`), then stays open as the second step of a small wizard: it
 * follows the new bridge through the page's own polling of the instance list, and when the
 * bridge is ready shows the sign-in steps for that person (`InstanceNextSteps`), so the operator
 * who added it knows what to tell them. The same steps stay under the table after Done.
 */
export function AddInstanceDialog({
  offering,
  type,
  open,
  onOpenChange,
  serverName,
}: {
  offering: BridgeOffering;
  type?: BridgeType;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  serverName?: string;
}) {
  const put = usePutBridgeInstance();
  const [userId, setUserId] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [added, setAdded] = useState<BridgeInstance | null>(null);
  const name = offering.name ?? offering.type;
  // The page polls this list while any bridge is on its way; the dialog reads the same answer.
  const { data: instances } = useBridgeInstances(added ? offering.type : undefined);
  const live = added ? (instances?.find((i) => i.user_id === added.user_id) ?? added) : null;

  function handleOpenChange(next: boolean) {
    if (!next) {
      setUserId("");
      setError(null);
      setAdded(null);
    }
    onOpenChange(next);
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    const id = userId.trim();
    if (!looksLikeUserId(id)) {
      setError(`A Matrix ID, like @alice:${serverName ?? "example.org"}.`);
      return;
    }
    setError(null);
    try {
      const instance = await put.mutateAsync({ type: offering.type, userId: id });
      toast({ title: `Setting up ${id}'s ${name} bridge` });
      setAdded(instance);
    } catch (err) {
      setError(
        err instanceof ApiProblemError
          ? (err.problem.detail ?? err.problem.title ?? "The server refused.")
          : "Couldn’t reach the server.",
      );
    }
  }

  if (live) {
    return (
      <Dialog open={open} onOpenChange={handleOpenChange}>
        <DialogContent
          size="form"
          title={`${live.user_id}'s ${name} bridge`}
          description={
            live.state === "ready"
              ? "It is running. Here is what they do next."
              : "Watch it come up here, or close this: the same next steps stay under the table."
          }
          footer={
            <Button type="button" onClick={() => handleOpenChange(false)}>
              Done
            </Button>
          }
        >
          <InstanceNextSteps instance={live} offering={offering} type={type} />
        </DialogContent>
      </Dialog>
    );
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        size="form"
        title={`Add a ${name} bridge for someone`}
        description={
          offering.runtime === "cluster"
            ? "This server starts their bridge now and invites them to it when it's running, as if they had messaged its bot."
            : "It is registered now and waits for its first ping: download its files from the table and run it where it can run."
        }
      >
        <form onSubmit={handleSubmit} noValidate>
          <Field label="Matrix ID" required error={error ?? undefined}>
            {(f) => (
              <Input
                {...f}
                value={userId}
                autoComplete="off"
                placeholder={`@alice:${serverName ?? "example.org"}`}
                onChange={(e) => setUserId(e.target.value)}
                className="font-identifier"
              />
            )}
          </Field>
          <div className="mt-6 flex justify-end gap-2">
            <Button type="button" variant="secondary" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" disabled={put.isPending}>
              {put.isPending ? "Adding..." : "Add bridge"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
