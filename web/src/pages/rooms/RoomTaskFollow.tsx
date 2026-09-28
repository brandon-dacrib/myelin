import { useEffect, useRef } from "react";
import { Link } from "@tanstack/react-router";
import { useTask } from "@/api/tasks";
import { describeProgress, progressFraction } from "@/lib/tasks";
import { describeRoomTaskResult } from "@/lib/rooms";
import { TaskProgressBar, TaskStatus } from "@/pages/tasks/TaskProgress";

/**
 * Follows a task a room operation started (purge, delete, quarantine): its status, how far it
 * has got, and once it has succeeded, what it did. `onDone` is called once, when it ends.
 */
export function RoomTaskFollow({
  taskId,
  onDone,
}: {
  taskId: string;
  onDone?: (status: string) => void;
}) {
  const task = useTask(taskId);
  const reported = useRef(false);
  const status = task.data?.status;
  useEffect(() => {
    if (!status || reported.current) return;
    if (status === "succeeded" || status === "failed" || status === "cancelled") {
      reported.current = true;
      onDone?.(status);
    }
  }, [status, onDone]);

  if (!task.data) {
    return <p className="text-sm text-text-muted">Starting the task…</p>;
  }
  const words = describeProgress(task.data);
  const summary = describeRoomTaskResult(task.data);
  return (
    <div className="flex flex-col gap-2 rounded-md border border-border p-3" aria-live="polite">
      <div className="flex items-center justify-between gap-3">
        <TaskStatus task={task.data} />
        <Link
          to="/tasks/$taskId"
          params={{ taskId }}
          search={{}}
          className="text-xs text-accent underline underline-offset-2 hover:no-underline"
        >
          Open the task
        </Link>
      </div>
      {task.data.status !== "running" && task.data.progress && (
        <TaskProgressBar
          fraction={progressFraction(task.data)}
          label={words ?? task.data.progress.message ?? "Done"}
          compact
        />
      )}
      {task.data.progress?.message && task.data.status === "running" && (
        <p className="text-xs text-text-muted">{task.data.progress.message}</p>
      )}
      {summary && <p className="text-sm text-text">{summary}</p>}
      {task.data.status === "failed" && (
        <p role="alert" className="text-sm text-danger">
          {task.data.error?.detail ?? task.data.error?.title ?? "The task failed."}
        </p>
      )}
    </div>
  );
}
