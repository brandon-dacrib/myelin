import type { ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import { useTask, type Task } from "@/api/tasks";
import { TaskProgressBar, TaskStatus } from "@/pages/tasks/TaskProgress";
import { describeProgress, progressFraction } from "@/lib/tasks";
import { QueryProblemState } from "@/components/QueryProblemState";

/**
 * Follows one Task an action on this page started (`GET /tasks/{id}`, polled while it runs):
 * its status, how far it has got, and once it ends, what it did. Links to the task's own page
 * on the Tasks page, where it stays after this page is gone.
 */
export function FollowTask({
  taskId,
  title,
  result,
}: {
  taskId: string;
  /** What the task is doing, as a heading: "Redacting messages". */
  title: string;
  /** What a succeeded task did, in words. */
  result: (task: Task) => ReactNode;
}) {
  const { data: task, isError, error, refetch } = useTask(taskId);
  const headingId = `follow-task-${taskId}`;

  return (
    <section
      aria-labelledby={headingId}
      aria-live="polite"
      className="rounded-md border border-border bg-surface p-3"
    >
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h4 id={headingId} className="text-sm font-medium text-text">
          {title}
        </h4>
        <Link
          to="/tasks/$taskId"
          params={{ taskId }}
          search={{}}
          className="text-xs text-accent underline underline-offset-2 hover:no-underline"
        >
          Open in Tasks
        </Link>
      </div>
      {isError ? (
        <QueryProblemState error={error} resource="this task" onRetry={() => refetch()} compact />
      ) : !task ? (
        <p className="mt-2 text-sm text-text-muted">Starting…</p>
      ) : (
        <div className="mt-2 flex flex-col gap-2">
          {task.status === "running" || task.status === "scheduled" ? (
            <TaskProgressBar
              fraction={progressFraction(task)}
              label={describeProgress(task) ?? "Working"}
            />
          ) : (
            <TaskStatus task={task} />
          )}
          {task.status === "succeeded" && <div className="text-sm text-text">{result(task)}</div>}
          {task.status === "failed" && (
            <p role="alert" className="text-sm text-danger">
              {task.error?.detail ?? task.error?.title ?? "The task failed."}
            </p>
          )}
          {task.status === "cancelled" && (
            <p className="text-sm text-text-muted">Cancelled before it finished.</p>
          )}
        </div>
      )}
    </section>
  );
}
