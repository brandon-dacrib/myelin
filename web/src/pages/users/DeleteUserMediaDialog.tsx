import { useDeleteUserMedia, useUserStatistics } from "@/api/user-moderation";
import type { Task } from "@/api/tasks";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogContent } from "@/components/ui/dialog/Dialog";
import { MutationError } from "@/components/MutationError";
import { formatBytes } from "@/lib/format";

interface DeleteUserMediaDialogProps {
  userId: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** Called with the Task the server started, for the page to follow. */
  onStarted: (task: Task) => void;
}

/**
 * Deletes everything a user uploaded (`DELETE /users/{user_id}/media`), except items marked
 * protected. Names the user and, when the server can count it, how much that is.
 */
export function DeleteUserMediaDialog({
  userId,
  open,
  onOpenChange,
  onStarted,
}: DeleteUserMediaDialogProps) {
  const del = useDeleteUserMedia();
  const { data: stats } = useUserStatistics(open ? userId : undefined);

  function handleOpenChange(next: boolean) {
    if (!next) del.reset();
    onOpenChange(next);
  }

  async function handleConfirm() {
    try {
      const task = await del.mutateAsync({ userId });
      onStarted(task);
      handleOpenChange(false);
    } catch {
      /* shown below from del.error */
    }
  }

  const amount =
    stats?.media_count != null
      ? `${stats.media_count.toLocaleString()} ${stats.media_count === 1 ? "file" : "files"}${
          stats.media_bytes != null ? `, ${formatBytes(stats.media_bytes)}` : ""
        }`
      : null;

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent
        title={`Delete all media uploaded by ${userId}?`}
        description="Every file they uploaded is deleted from this server, except ones marked protected. Messages that showed them show nothing. This cannot be undone."
        footer={
          <>
            <Button variant="secondary" onClick={() => handleOpenChange(false)}>
              Cancel
            </Button>
            <Button variant="danger" disabled={del.isPending} onClick={handleConfirm}>
              {del.isPending ? "Deleting…" : "Delete all media"}
            </Button>
          </>
        }
      >
        {amount && <p className="text-sm text-text">They have uploaded {amount}.</p>}
        {del.error != null && (
          <MutationError error={del.error} action="delete their media" className="mt-3" />
        )}
      </DialogContent>
    </Dialog>
  );
}
