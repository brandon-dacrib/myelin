import { useState, type FormEvent } from "react";
import { usePutBridgeInstance, type BridgeOffering } from "@/api/bridges";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { toast } from "@/components/ui/toast/toast-store";
import { looksLikeUserId } from "@/lib/bridge-offerings";

/**
 * Sets up a bridge for someone, exactly as their message to the front door would
 * (`PUT .../instances/{user_id}`). They are invited to it when it is ready.
 */
export function AddInstanceDialog({
  offering,
  open,
  onOpenChange,
  serverName,
}: {
  offering: BridgeOffering;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  serverName?: string;
}) {
  const put = usePutBridgeInstance();
  const [userId, setUserId] = useState("");
  const [error, setError] = useState<string | null>(null);
  const name = offering.name ?? offering.type;

  function handleOpenChange(next: boolean) {
    if (!next) {
      setUserId("");
      setError(null);
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
      await put.mutateAsync({ type: offering.type, userId: id });
      toast({ title: `Setting up ${id}'s ${name} bridge` });
      handleOpenChange(false);
    } catch (err) {
      setError(
        err instanceof ApiProblemError
          ? (err.problem.detail ?? err.problem.title ?? "The server refused.")
          : "Couldn’t reach the server.",
      );
    }
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
