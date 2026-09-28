import { useState } from "react";
import { useAddRoomAlias, useRemoveRoomAlias, useRoomAliases } from "@/api/room-contents";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { Field, Input } from "@/components/ui/input/Input";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { QueryProblemState } from "@/components/QueryProblemState";
import { MutationError } from "@/components/MutationError";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";

/** The room's local aliases: which one is canonical, adding one, and removing one. */
export function RoomAliasesTab({ roomId }: { roomId: string }) {
  const aliases = useRoomAliases(roomId);
  const add = useAddRoomAlias();
  const remove = useRemoveRoomAlias();
  const [alias, setAlias] = useState("");
  const [removing, setRemoving] = useState<string | null>(null);
  const canWrite = hasScope("moderation:write");

  return (
    <section aria-labelledby="room-aliases-heading">
      <h2 id="room-aliases-heading" className="text-md font-medium text-text">
        Aliases
      </h2>
      {aliases.isLoading ? (
        <SkeletonText lines={2} />
      ) : aliases.isError ? (
        <QueryProblemState
          error={aliases.error}
          resource="this room's aliases"
          onRetry={() => aliases.refetch()}
        />
      ) : (aliases.data?.length ?? 0) === 0 ? (
        <p className="mt-3 text-sm text-text-muted">This room has no local aliases.</p>
      ) : (
        <ul
          aria-label="Aliases"
          className="mt-3 divide-y divide-border rounded-md border border-border"
        >
          {aliases.data?.map((a) => (
            <li key={a.alias} className="flex items-center justify-between gap-3 px-4 py-3">
              <span className="flex flex-wrap items-center gap-2">
                <span className="font-identifier text-text">{a.alias}</span>
                {a.canonical && (
                  <Badge status="info" hideIcon>
                    Canonical
                  </Badge>
                )}
                {a.creator && <span className="text-xs text-text-muted">by {a.creator}</span>}
              </span>
              <Button
                size="sm"
                variant="secondary"
                disabled={!canWrite}
                title={canWrite ? undefined : "Needs moderation:write"}
                aria-label={`Remove ${a.alias}`}
                onClick={() => setRemoving(a.alias)}
              >
                Remove
              </Button>
            </li>
          ))}
        </ul>
      )}

      <form
        className="mt-4 flex max-w-lg flex-wrap items-end gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          const value = alias.trim();
          if (!value) return;
          add.mutate(
            { roomId, alias: value },
            {
              onSuccess: () => {
                toast({ title: `${value} now points here` });
                setAlias("");
              },
            },
          );
        }}
      >
        <Field label="New alias">
          {(field) => (
            <Input
              {...field}
              placeholder="#name:your.server"
              value={alias}
              onChange={(e) => setAlias(e.target.value)}
              disabled={!canWrite}
            />
          )}
        </Field>
        <Button type="submit" disabled={!canWrite || !alias.trim() || add.isPending}>
          Add alias
        </Button>
      </form>
      {add.isError && <MutationError className="mt-2" error={add.error} action="add the alias" />}

      <Dialog
        open={removing !== null}
        onOpenChange={(open) => {
          if (!open) {
            setRemoving(null);
            remove.reset();
          }
        }}
      >
        {removing && (
          <DialogContent
            title={`Remove ${removing}?`}
            description="The alias stops resolving to this room. The room itself is not changed; if it is the canonical alias, the room keeps naming it until someone changes that."
            footer={
              <>
                <DialogClose asChild>
                  <Button variant="secondary">Cancel</Button>
                </DialogClose>
                <Button
                  variant="danger"
                  disabled={remove.isPending}
                  onClick={() =>
                    remove.mutate(
                      { roomId, alias: removing },
                      {
                        onSuccess: () => {
                          toast({ title: `${removing} removed` });
                          setRemoving(null);
                        },
                      },
                    )
                  }
                >
                  Remove alias
                </Button>
              </>
            }
          >
            {remove.isError && <MutationError error={remove.error} action="remove the alias" />}
          </DialogContent>
        )}
      </Dialog>
    </section>
  );
}
