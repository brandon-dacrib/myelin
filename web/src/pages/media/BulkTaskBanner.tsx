import { useEffect, useRef } from "react";
import { Link } from "@tanstack/react-router";
import { useQueryClient } from "@tanstack/react-query";
import { invalidateMedia } from "@/api/media";
import { taskIsActive, useCancelTask, useTask, useTasks } from "@/api/tasks";
import { Button } from "@/components/ui/button/Button";
import { hasScope } from "@/lib/auth";
import { describeProgress, describeTaskAction, progressFraction } from "@/lib/tasks";
import { TaskProgressBar } from "../tasks/TaskProgress";
import { announceBulkTask, BULK_MEDIA_ACTIONS } from "./bulk-tasks";

/**
 * One running bulk deletion: what it is, how far it has got, and a way to stop it. Followed
 * through `useTask` (the event stream while it is connected, polling otherwise); when it ends,
 * the media list is refetched, the outcome is announced and `onEnded` is called.
 */
function BulkTaskRow({ taskId, onEnded }: { taskId: string; onEnded?: (id: string) => void }) {
  const qc = useQueryClient();
  const { data: task } = useTask(taskId);
  const cancel = useCancelTask();
  const ended = useRef(false);

  useEffect(() => {
    if (!task || taskIsActive(task) || ended.current) return;
    ended.current = true;
    invalidateMedia(qc);
    // Only what was started from this page is announced; one started elsewhere just goes.
    if (onEnded) {
      announceBulkTask(task);
      onEnded(task.id);
    }
  }, [task, qc, onEnded]);

  if (!task || !taskIsActive(task)) return null;
  const name = describeTaskAction(task.action);
  const words = describeProgress(task);
  return (
    <li
      aria-label={name}
      className="flex flex-wrap items-center gap-4 rounded-md border border-border bg-surface px-4 py-3"
    >
      <div className="min-w-[14rem] flex-1">
        <p className="text-sm font-medium text-text">
          {name}{" "}
          <Link
            to="/tasks/$taskId"
            params={{ taskId: task.id }}
            className="text-xs font-normal text-text-muted underline-offset-2 hover:underline"
          >
            View task
          </Link>
        </p>
        <TaskProgressBar
          fraction={progressFraction(task)}
          label={words ? `${words} checked` : "Starting"}
        />
      </div>
      {hasScope("admin:write") && (
        <Button
          variant="secondary"
          disabled={cancel.isPending}
          onClick={() => cancel.mutate(task.id)}
          aria-label={`Stop ${name.toLowerCase()}`}
        >
          Stop
        </Button>
      )}
    </li>
  );
}

/**
 * The bulk deletions in progress: those started from this page (`followed`, announced when
 * they end) and any other running one the server lists (started elsewhere, or before a reload).
 */
export function BulkTaskBanner({
  followed,
  onEnded,
}: {
  followed: string[];
  onEnded: (id: string) => void;
}) {
  const running = useTasks({ status: "running", action: "media." }).data?.items ?? [];
  const others = running
    .filter((t) => (BULK_MEDIA_ACTIONS as readonly string[]).includes(t.action))
    .map((t) => t.id)
    .filter((id) => !followed.includes(id));
  if (followed.length === 0 && others.length === 0) return null;
  return (
    <section aria-label="Bulk deletions in progress" className="mt-4">
      <ul className="flex flex-col gap-2">
        {followed.map((id) => (
          <BulkTaskRow key={id} taskId={id} onEnded={onEnded} />
        ))}
        {others.map((id) => (
          <BulkTaskRow key={id} taskId={id} />
        ))}
      </ul>
    </section>
  );
}
