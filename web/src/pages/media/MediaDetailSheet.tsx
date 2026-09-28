import type { ReactNode } from "react";
import {
  mediaKey,
  mediaName,
  useDeleteMedia,
  useMediaFlagAction,
  useMediaItem,
  type MediaFlagAction,
  type MediaItem,
} from "@/api/media";
import { ApiProblemError } from "@/api/problem";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent, DialogTrigger } from "@/components/ui/dialog/Dialog";
import { Sheet, SheetContent } from "@/components/ui/sheet/Sheet";
import { toast } from "@/components/ui/toast/toast-store";
import { CopyableId } from "@/components/CopyableId";
import { RelativeTime } from "@/components/RelativeTime";
import { hasScope } from "@/lib/auth";
import { formatBytes } from "@/lib/format";
import { MediaThumbnail } from "./MediaThumbnail";

const DONE: Record<MediaFlagAction, string> = {
  quarantine: "Quarantined",
  unquarantine: "Quarantine lifted",
  protect: "Protected",
  unprotect: "No longer protected",
};

function errorDetail(error: Error): string | undefined {
  return error instanceof ApiProblemError ? error.problem.detail : error.message;
}

/**
 * One media item: a preview, what is known about it, and what can be done to it. Opened from
 * the list with the row's data and kept fresh from `GET /media/{server}/{id}`, so the badges
 * follow each action.
 */
export function MediaDetailSheet({
  selected,
  onClose,
}: {
  selected: MediaItem | null;
  onClose: () => void;
}) {
  const { data } = useMediaItem(selected ?? undefined);
  // After a deletion the item is gone; the row it was opened from keeps the sheet readable
  // while it closes.
  const item = data ?? selected;
  const canModerate = hasScope("moderation:write");
  const flag = useMediaFlagAction();
  const remove = useDeleteMedia();

  function act(action: MediaFlagAction) {
    if (!item) return;
    flag.mutate(
      { item, action },
      {
        onSuccess: () => toast({ title: `${DONE[action]}: ${mediaName(item)}` }),
        onError: (error) =>
          toast({
            title: `Couldn't ${action} ${mediaName(item)}`,
            description: errorDetail(error),
            variant: "danger",
          }),
      },
    );
  }

  return (
    <Sheet open={selected !== null} onOpenChange={(open) => !open && onClose()}>
      {item && (
        <SheetContent
          title={mediaName(item)}
          description={
            item.origin === "local" ? "Uploaded here" : `Cached from ${item.server_name}`
          }
          className="max-w-md"
        >
          <MediaThumbnail item={item} size="detail" />
          <div className="mt-3 flex flex-wrap gap-1">
            {item.quarantined && <Badge status="danger">Quarantined</Badge>}
            {item.protected && <Badge status="success">Protected</Badge>}
            <Badge status="neutral" hideIcon>
              {item.origin === "local" ? "Local" : "Remote"}
            </Badge>
          </div>

          <dl className="mt-4 grid grid-cols-1 gap-3">
            <Fact label="Content URI" value={<CopyableId value={mediaKey(item)} />} />
            <Fact label="Uploader" value={item.uploader ?? "—"} />
            <Fact label="Type" value={item.content_type ?? "Unknown"} />
            <Fact label="Size" value={formatBytes(item.size_bytes)} />
            <Fact label="Uploaded" value={<RelativeTime at={item.created_at} />} />
            <Fact label="Last viewed" value={<RelativeTime at={item.last_accessed_at} />} />
          </dl>

          <h3 className="mt-6 text-sm font-medium text-text">Actions</h3>
          {!canModerate && (
            <p className="mt-1 text-xs text-text-muted">Acting on media needs moderation:write.</p>
          )}
          <div className="mt-2 flex flex-col gap-3">
            {item.quarantined ? (
              <Action
                hint="Lets people fetch it again."
                button={
                  <Button
                    variant="secondary"
                    disabled={!canModerate || flag.isPending}
                    onClick={() => act("unquarantine")}
                  >
                    Lift quarantine
                  </Button>
                }
              />
            ) : (
              <Action
                hint={
                  item.protected
                    ? "Protected media can't be quarantined; unprotect it first."
                    : "Withholds it from everyone but administrators, without deleting it."
                }
                button={
                  <Dialog>
                    <DialogTrigger asChild>
                      <Button variant="secondary" disabled={!canModerate || item.protected}>
                        Quarantine
                      </Button>
                    </DialogTrigger>
                    <DialogContent
                      title={`Quarantine ${mediaName(item)}?`}
                      description="Nobody but an administrator can view or download it until the quarantine is lifted. Rooms that show it will show it as missing."
                      footer={
                        <>
                          <DialogClose asChild>
                            <Button variant="secondary">Cancel</Button>
                          </DialogClose>
                          <DialogClose asChild>
                            <Button variant="danger" onClick={() => act("quarantine")}>
                              Quarantine
                            </Button>
                          </DialogClose>
                        </>
                      }
                    />
                  </Dialog>
                }
              />
            )}
            <Action
              hint={
                item.protected
                  ? "Bulk deletions and quarantine skip it."
                  : item.quarantined
                    ? "Quarantined media can't be protected; lift the quarantine first."
                    : "Keeps it from bulk deletions and quarantine."
              }
              button={
                <Button
                  variant="secondary"
                  disabled={!canModerate || flag.isPending || (!item.protected && item.quarantined)}
                  onClick={() => act(item.protected ? "unprotect" : "protect")}
                >
                  {item.protected ? "Unprotect" : "Protect"}
                </Button>
              }
            />
            <Action
              hint="Removes the file and its thumbnails for good."
              button={
                <Dialog>
                  <DialogTrigger asChild>
                    <Button variant="danger" disabled={!canModerate}>
                      Delete
                    </Button>
                  </DialogTrigger>
                  <DialogContent
                    title={`Delete ${mediaName(item)}?`}
                    description={
                      item.origin === "local"
                        ? `The file (${formatBytes(item.size_bytes)}) and its thumbnails are removed from this server for good. Messages that show it will show it as missing. This cannot be undone.`
                        : `This server's copy (${formatBytes(item.size_bytes)}) is removed. ${item.server_name} still has the original, and it is fetched again if someone here views it.`
                    }
                    footer={
                      <>
                        <DialogClose asChild>
                          <Button variant="secondary">Cancel</Button>
                        </DialogClose>
                        <DialogClose asChild>
                          <Button
                            variant="danger"
                            onClick={() =>
                              remove.mutate(item, {
                                onSuccess: () => {
                                  toast({ title: `Deleted ${mediaName(item)}` });
                                  onClose();
                                },
                                onError: (error) =>
                                  toast({
                                    title: `Couldn't delete ${mediaName(item)}`,
                                    description: errorDetail(error),
                                    variant: "danger",
                                  }),
                              })
                            }
                          >
                            Delete
                          </Button>
                        </DialogClose>
                      </>
                    }
                  />
                </Dialog>
              }
            />
          </div>
        </SheetContent>
      )}
    </Sheet>
  );
}

function Action({ hint, button }: { hint: string; button: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-3">
      <p className="text-xs text-text-muted">{hint}</p>
      {button}
    </div>
  );
}

function Fact({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div>
      <dt className="text-xs text-text-muted">{label}</dt>
      <dd className="mt-0.5 break-all text-sm text-text">{value}</dd>
    </div>
  );
}
