import { useState } from "react";
import { useQuarantineRoomMedia, useRefreshRoom, useRoomMedia } from "@/api/room-contents";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { QueryProblemState } from "@/components/QueryProblemState";
import { MutationError } from "@/components/MutationError";
import { hasScope } from "@/lib/auth";
import { formatBytes } from "@/lib/format";
import { RoomTaskFollow } from "./RoomTaskFollow";

/** The media the room's events refer to that this server holds, and quarantining all of it. */
export function RoomMediaTab({ roomId }: { roomId: string }) {
  const media = useRoomMedia(roomId);
  const quarantine = useQuarantineRoomMedia();
  const refresh = useRefreshRoom();
  const [confirming, setConfirming] = useState(false);
  const [taskId, setTaskId] = useState<string | null>(null);
  const canWrite = hasScope("moderation:write");
  const items = media.data?.items ?? [];

  return (
    <section aria-labelledby="room-media-heading">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 id="room-media-heading" className="text-md font-medium text-text">
          Media
        </h2>
        <Button
          variant="danger"
          disabled={!canWrite || items.length === 0}
          title={canWrite ? undefined : "Needs moderation:write"}
          onClick={() => setConfirming(true)}
        >
          Quarantine all
        </Button>
      </div>
      {taskId && (
        <div className="mt-3">
          <RoomTaskFollow taskId={taskId} onDone={() => refresh(roomId)} />
        </div>
      )}
      {media.isLoading ? (
        <SkeletonText lines={3} />
      ) : media.isError ? (
        <QueryProblemState
          error={media.error}
          resource="this room's media"
          onRetry={() => media.refetch()}
        />
      ) : items.length === 0 ? (
        <p className="mt-3 text-sm text-text-muted">
          No media this server holds is referred to from this room.
        </p>
      ) : (
        <ul
          aria-label="Room media"
          className="mt-3 divide-y divide-border rounded-md border border-border"
        >
          {items.map((item) => (
            <li
              key={`${item.server_name}/${item.media_id}`}
              className="flex flex-wrap items-center justify-between gap-3 px-4 py-3"
            >
              <span className="flex flex-col">
                <span className="text-text">{item.upload_name ?? item.media_id}</span>
                <span className="font-identifier text-xs text-text-muted">
                  mxc://{item.server_name}/{item.media_id}
                </span>
              </span>
              <span className="flex flex-wrap items-center gap-2 text-xs text-text-muted">
                {formatBytes(item.size_bytes)}
                {item.quarantined && <Badge status="danger">Quarantined</Badge>}
                {item.protected && <Badge status="info">Protected</Badge>}
              </span>
            </li>
          ))}
        </ul>
      )}

      <Dialog
        open={confirming}
        onOpenChange={(open) => {
          if (!open) {
            setConfirming(false);
            quarantine.reset();
          }
        }}
      >
        <DialogContent
          title="Quarantine all of this room's media?"
          description="Every item this room refers to stops being served to anyone. Protected items are left alone. It runs as a task."
          footer={
            <>
              <DialogClose asChild>
                <Button variant="secondary">Cancel</Button>
              </DialogClose>
              <Button
                variant="danger"
                disabled={quarantine.isPending}
                onClick={() =>
                  quarantine.mutate(roomId, {
                    onSuccess: (task) => {
                      setTaskId(task.id);
                      setConfirming(false);
                    },
                  })
                }
              >
                Quarantine media
              </Button>
            </>
          }
        >
          {quarantine.isError && (
            <MutationError error={quarantine.error} action="quarantine the media" />
          )}
        </DialogContent>
      </Dialog>
    </section>
  );
}
