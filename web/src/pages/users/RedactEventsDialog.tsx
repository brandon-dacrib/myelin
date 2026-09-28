import { useState, type FormEvent } from "react";
import { useRedactUserEvents, useUserMemberships } from "@/api/user-moderation";
import type { Task } from "@/api/tasks";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { Field, Input, Textarea } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { MutationError } from "@/components/MutationError";

const EVERY_ROOM = "__every_room__";

interface RedactEventsDialogProps {
  userId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Called with the Task the server started, for the page to follow. */
  onStarted: (task: Task) => void;
}

/**
 * Redacts what a user sent (`POST /users/{user_id}/redact-events`), in every room or one,
 * newest first, optionally only the most recent N. The server does it as a Task; this closes
 * once it has started and the page follows the task.
 */
export function RedactEventsDialog({
  userId,
  open,
  onOpenChange,
  onStarted,
}: RedactEventsDialogProps) {
  const redact = useRedactUserEvents();
  const { data: memberships } = useUserMemberships(open ? userId : undefined);
  const [roomId, setRoomId] = useState(EVERY_ROOM);
  const [reason, setReason] = useState("");
  const [limit, setLimit] = useState("");
  const [limitError, setLimitError] = useState<string>();

  const roomOptions = [
    { value: EVERY_ROOM, label: "Every room" },
    ...(memberships?.items ?? [])
      .filter((m) => m.room_id)
      .map((m) => ({
        value: m.room_id!,
        label: m.room_name ? `${m.room_name} (${m.room_id})` : m.room_id!,
      })),
  ];

  function handleOpenChange(next: boolean) {
    if (!next) {
      setRoomId(EVERY_ROOM);
      setReason("");
      setLimit("");
      setLimitError(undefined);
      redact.reset();
    }
    onOpenChange(next);
  }

  async function handleSubmit(e: FormEvent) {
    e.preventDefault();
    const limitValue = limit.trim() === "" ? undefined : Number(limit);
    if (limitValue !== undefined && (!Number.isInteger(limitValue) || limitValue < 1)) {
      setLimitError("A whole number, 1 or more, or leave it empty for all of them.");
      return;
    }
    setLimitError(undefined);
    try {
      const task = await redact.mutateAsync({
        userId,
        roomId: roomId === EVERY_ROOM ? undefined : roomId,
        reason: reason.trim(),
        limit: limitValue,
      });
      onStarted(task);
      handleOpenChange(false);
    } catch {
      /* shown below from redact.error */
    }
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        size="form"
        title={`Redact messages sent by ${userId}?`}
        description="Removes the content of what they sent, newest first. Everyone in those rooms sees it removed. This cannot be undone."
      >
        <form onSubmit={handleSubmit} className="flex flex-col gap-4" noValidate>
          <Field label="Rooms">
            {(fieldProps) => (
              <Select
                id={fieldProps.id}
                value={roomId}
                onValueChange={setRoomId}
                options={roomOptions}
              />
            )}
          </Field>
          <Field
            label="Only the most recent"
            hint="Leave empty to redact everything they sent."
            error={limitError}
          >
            {(fieldProps) => (
              <Input
                {...fieldProps}
                type="number"
                inputMode="numeric"
                min={1}
                step={1}
                value={limit}
                onChange={(e) => setLimit(e.target.value)}
              />
            )}
          </Field>
          <Field label="Reason" hint="Sent with each redaction, and recorded in the audit log.">
            {(fieldProps) => (
              <Textarea
                {...fieldProps}
                value={reason}
                maxLength={500}
                onChange={(e) => setReason(e.target.value)}
              />
            )}
          </Field>
          {redact.error != null && <MutationError error={redact.error} action="start redacting" />}
          <div className="flex justify-end gap-2">
            <Button type="button" variant="secondary" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button type="submit" variant="danger" disabled={redact.isPending}>
              {redact.isPending ? "Starting…" : "Redact messages"}
            </Button>
          </div>
        </form>
      </DialogContent>
    </Dialog>
  );
}
