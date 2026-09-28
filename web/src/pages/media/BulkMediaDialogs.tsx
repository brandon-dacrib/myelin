import { useState } from "react";
import { Eraser, Trash2 } from "lucide-react";
import { useBulkDeleteMedia, usePurgeRemoteMediaCache, type Task } from "@/api/media";
import { taskIsActive } from "@/api/tasks";
import { ApiProblemError } from "@/api/problem";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent, DialogTrigger } from "@/components/ui/dialog/Dialog";
import { Field, Input } from "@/components/ui/input/Input";
import { toast } from "@/components/ui/toast/toast-store";
import { formatBytes } from "@/lib/format";
import { describeTaskAction } from "@/lib/tasks";
import { announceBulkTask } from "./bulk-tasks";

/** `YYYY-MM-DD`, `days` before today. */
function daysAgo(days: number): string {
  return new Date(Date.now() - days * 86_400_000).toISOString().slice(0, 10);
}

/** How a date picked in the form reads in a sentence. */
function readable(date: string): string {
  return new Date(`${date}T00:00:00Z`).toLocaleDateString(undefined, {
    year: "numeric",
    month: "long",
    day: "numeric",
    timeZone: "UTC",
  });
}

/**
 * What a started bulk deletion does next: one that has already ended (a server with no task
 * registry answers it finished) is announced at once; a running one is handed to the page,
 * which follows it.
 */
function started(task: Task, onStarted: (task: Task) => void) {
  if (taskIsActive(task)) {
    toast({
      title: `${describeTaskAction(task.action)} started`,
      description: "Its progress is shown on this page.",
    });
    onStarted(task);
  } else {
    announceBulkTask(task);
  }
}

function problemToast(title: string) {
  return (error: Error) =>
    toast({
      title,
      description: error instanceof ApiProblemError ? error.problem.detail : error.message,
      variant: "danger",
    });
}

/**
 * "Delete old media": this server's own uploads nobody has fetched since a date, optionally
 * only the large ones. States exactly what it will delete, and what it keeps, before it does.
 */
export function BulkDeleteDialog({
  disabled,
  onStarted,
}: {
  disabled: boolean;
  /** Called with the task a deletion that is still running is answered with. */
  onStarted: (task: Task) => void;
}) {
  const [open, setOpen] = useState(false);
  const [before, setBefore] = useState(() => daysAgo(90));
  const [minMb, setMinMb] = useState("");
  const bulkDelete = useBulkDeleteMedia();
  const minBytes = minMb.trim() === "" ? undefined : Math.round(Number(minMb) * 1024 * 1024);
  const minInvalid = minBytes !== undefined && (!Number.isFinite(minBytes) || minBytes < 0);

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button
          variant="secondary"
          disabled={disabled}
          title={disabled ? "Needs moderation:write" : undefined}
        >
          <Trash2 size={16} aria-hidden="true" />
          Delete old media
        </Button>
      </DialogTrigger>
      <DialogContent
        size="form"
        title="Delete old media"
        description="Deletes uploads from this server's users that nobody has viewed or downloaded since the date you pick. Protected media is kept. This cannot be undone."
        footer={
          <>
            <DialogClose asChild>
              <Button variant="secondary">Cancel</Button>
            </DialogClose>
            <Button
              variant="danger"
              disabled={!before || minInvalid || bulkDelete.isPending}
              onClick={() =>
                bulkDelete.mutate(
                  {
                    before: `${before}T00:00:00Z`,
                    ...(minBytes ? { min_size_bytes: minBytes } : {}),
                  },
                  {
                    onSuccess: (task) => {
                      setOpen(false);
                      started(task, onStarted);
                    },
                    onError: problemToast("Couldn't delete media"),
                  },
                )
              }
            >
              Delete media
            </Button>
          </>
        }
      >
        <div className="flex flex-col gap-4">
          <Field label="Not used since" required>
            {(props) => (
              <Input
                {...props}
                type="date"
                value={before}
                max={daysAgo(0)}
                onChange={(e) => setBefore(e.target.value)}
              />
            )}
          </Field>
          <Field
            label="Only files at least this large (MiB)"
            hint="Leave empty to include every size."
            error={minInvalid ? "Enter a size of zero or more." : undefined}
          >
            {(props) => (
              <Input
                {...props}
                type="number"
                min={0}
                step="0.1"
                inputMode="decimal"
                value={minMb}
                onChange={(e) => setMinMb(e.target.value)}
              />
            )}
          </Field>
          {before && (
            <p className="text-sm text-text">
              Media uploaded here and unused since {readable(before)}
              {minBytes ? `, ${formatBytes(minBytes)} or larger,` : ""} will be deleted for good.
            </p>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}

/**
 * "Purge remote cache": drops this server's copies of other servers' media. They are fetched
 * again from their origin the next time somebody here looks at them, so this frees space rather
 * than removing anything anyone loses.
 */
export function PurgeRemoteCacheDialog({
  disabled,
  onStarted,
}: {
  disabled: boolean;
  /** Called with the task a purge that is still running is answered with. */
  onStarted: (task: Task) => void;
}) {
  const [open, setOpen] = useState(false);
  const [before, setBefore] = useState(() => daysAgo(30));
  const [serverName, setServerName] = useState("");
  const purge = usePurgeRemoteMediaCache();
  const server = serverName.trim();

  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button
          variant="secondary"
          disabled={disabled}
          title={disabled ? "Needs admin:write" : undefined}
        >
          <Eraser size={16} aria-hidden="true" />
          Purge remote cache
        </Button>
      </DialogTrigger>
      <DialogContent
        size="form"
        title="Purge cached remote media"
        description="Removes this server's copies of media from other servers. Each one is fetched again from its own server the next time someone here views it. Protected and quarantined copies are kept."
        footer={
          <>
            <DialogClose asChild>
              <Button variant="secondary">Cancel</Button>
            </DialogClose>
            <Button
              variant="danger"
              disabled={!before || purge.isPending}
              onClick={() =>
                purge.mutate(
                  {
                    before: `${before}T00:00:00Z`,
                    ...(server ? { server_name: server } : {}),
                  },
                  {
                    onSuccess: (task) => {
                      setOpen(false);
                      started(task, onStarted);
                    },
                    onError: problemToast("Couldn't purge the cache"),
                  },
                )
              }
            >
              Purge cache
            </Button>
          </>
        }
      >
        <div className="flex flex-col gap-4">
          <Field label="Not used since" required>
            {(props) => (
              <Input
                {...props}
                type="date"
                value={before}
                max={daysAgo(0)}
                onChange={(e) => setBefore(e.target.value)}
              />
            )}
          </Field>
          <Field label="Only from server" hint="Leave empty for every server, e.g. matrix.org.">
            {(props) => (
              <Input
                {...props}
                value={serverName}
                placeholder="Every server"
                onChange={(e) => setServerName(e.target.value)}
              />
            )}
          </Field>
          {before && (
            <p className="text-sm text-text">
              Cached copies from {server || "every other server"} unused since {readable(before)}{" "}
              will be removed.
            </p>
          )}
        </div>
      </DialogContent>
    </Dialog>
  );
}
