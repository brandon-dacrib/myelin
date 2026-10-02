import type { ReactNode } from "react";
import { ChevronLeft } from "lucide-react";
import { Link, useParams, useSearch } from "@tanstack/react-router";
import { taskIsActive, useCancelTask, useTask, type Task } from "@/api/tasks";
import { CopyBlock } from "@/components/CopyBlock";
import { CopyableId } from "@/components/CopyableId";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { ResourceLink } from "@/components/ResourceLink";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent, DialogTrigger } from "@/components/ui/dialog/Dialog";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import { formatDuration } from "@/lib/format";
import {
  describeProgress,
  describeTaskAction,
  progressFraction,
  TASK_STATUS_META,
  describeResultKey,
} from "@/lib/tasks";
import { TaskProgressBar } from "./TaskProgress";

/** `/tasks/$taskId`: one task, polled every two seconds while it can still change. */
export function TaskDetailPage() {
  const { taskId } = useParams({ from: "/tasks/$taskId" });
  const search = useSearch({ from: "/tasks/$taskId" });
  const query = useTask(taskId);

  return (
    <div className="mx-auto max-w-5xl space-y-6 p-6">
      <Link
        to="/tasks"
        search={search}
        className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
      >
        <ChevronLeft size={14} aria-hidden="true" />
        Tasks
      </Link>
      {!hasScope("admin:read") ? (
        <ForbiddenState scope="admin:read" />
      ) : query.isLoading ? (
        <SkeletonText lines={6} />
      ) : query.isError || !query.data ? (
        <QueryProblemState
          error={query.error}
          resource="this task"
          scope="admin:read"
          onRetry={() => query.refetch()}
        />
      ) : (
        <TaskView task={query.data} />
      )}
    </div>
  );
}

function TaskView({ task }: { task: Task }) {
  const meta = TASK_STATUS_META[task.status];
  const words = describeProgress(task);
  const started = task.started_at ? Date.parse(task.started_at) : null;
  const finished = task.finished_at ? Date.parse(task.finished_at) : null;

  return (
    <>
      <header className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h1 className="text-xl text-text">{describeTaskAction(task.action)}</h1>
          <p className="mt-1 break-all text-sm text-text-muted">
            Admin API operation <span className="font-identifier">{task.action}</span>
          </p>
          <div className="mt-2">
            <Badge status={meta.status}>{meta.label}</Badge>
          </div>
        </div>
        {taskIsActive(task) && hasScope("admin:write") && <CancelTaskButton task={task} />}
      </header>

      {(task.status === "running" || (words && task.status !== "succeeded")) && (
        <section aria-labelledby="task-progress-heading" className="space-y-2">
          <h2 id="task-progress-heading" className="text-md font-medium text-text">
            Progress
          </h2>
          <div className="rounded-md border border-border bg-surface p-4">
            <TaskProgressBar
              fraction={progressFraction(task)}
              label={
                words ? `${words}${task.status === "running" ? "" : ` when it stopped`}` : "Working"
              }
            />
            {task.progress?.message && (
              <p className="mt-2 text-sm text-text">{task.progress.message}</p>
            )}
          </div>
        </section>
      )}

      {task.error && (
        <div role="alert" className="rounded-md border border-danger bg-danger-bg p-4 text-sm">
          <p className="font-medium text-danger">{task.error.title}</p>
          {task.error.detail && <p className="mt-1 text-text">{task.error.detail}</p>}
          {task.error.request_id && (
            <p className="mt-2 text-text-muted">
              Request ID <CopyableId value={task.error.request_id} />
            </p>
          )}
        </div>
      )}

      <dl className="grid gap-4 rounded-md border border-border bg-surface p-4 text-sm sm:grid-cols-2 [&_dd]:mt-1 [&_dd]:break-all [&_dd]:text-text [&_dt]:text-text-muted">
        {task.resource && (
          <Fact label="On">
            <ResourceLink target={task.resource} />
          </Fact>
        )}
        <Fact label="Started by">
          {task.created_by?.kind === "system" ? (
            "The server"
          ) : task.created_by?.kind === "user" ? (
            <ResourceLink target={{ type: "user", id: task.created_by.id }} />
          ) : (
            (task.created_by?.display_name ?? task.created_by?.id ?? "—")
          )}
        </Fact>
        <Fact label="Created">
          <RelativeTime at={task.created_at} />
        </Fact>
        {task.scheduled_for && task.status === "scheduled" && (
          <Fact label="Runs at">
            <time dateTime={task.scheduled_for}>
              {new Date(task.scheduled_for).toLocaleString()}
            </time>
          </Fact>
        )}
        {task.started_at && (
          <Fact label="Started">
            <RelativeTime at={task.started_at} />
          </Fact>
        )}
        {task.finished_at && (
          <Fact label="Finished">
            <RelativeTime at={task.finished_at} />
          </Fact>
        )}
        {started != null && finished != null && (
          <Fact label="Took">{formatDuration(finished - started)}</Fact>
        )}
        <Fact label="Task ID">
          <CopyableId value={task.id} />
        </Fact>
      </dl>

      {task.result != null && <TaskResult task={task} />}
    </>
  );
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <dt>{label}</dt>
      <dd>{children}</dd>
    </div>
  );
}

/**
 * What the task reported when it finished. A flat record of numbers and words is shown as
 * facts; anything else is shown as the server sent it, read-only.
 */
function TaskResult({ task }: { task: Task }) {
  const result = task.result;
  const flat =
    typeof result === "object" &&
    result !== null &&
    !Array.isArray(result) &&
    Object.values(result).every((v) => ["string", "number", "boolean"].includes(typeof v));
  return (
    <section aria-labelledby="task-result-heading" className="space-y-2">
      <h2 id="task-result-heading" className="text-md font-medium text-text">
        Result
      </h2>
      {flat ? (
        <dl className="grid gap-4 rounded-md border border-border bg-surface p-4 text-sm sm:grid-cols-3 [&_dd]:mt-1 [&_dd]:text-text [&_dt]:text-text-muted">
          {Object.entries(result as Record<string, string | number | boolean>).map(([k, v]) => (
            <Fact key={k} label={describeResultKey(k)}>
              {typeof v === "number" ? v.toLocaleString() : String(v)}
            </Fact>
          ))}
        </dl>
      ) : (
        <CopyBlock
          label="Result (JSON)"
          content={JSON.stringify(result, null, 2)}
          filename={`task-${task.id}-result.json`}
        />
      )}
    </section>
  );
}

function CancelTaskButton({ task }: { task: Task }) {
  const cancel = useCancelTask();
  const name = describeTaskAction(task.action).toLowerCase();
  return (
    <Dialog>
      <DialogTrigger asChild>
        <Button variant="secondary">Cancel task</Button>
      </DialogTrigger>
      <DialogContent
        title={`Cancel ${name}?`}
        description={
          task.status === "scheduled"
            ? "It will not run."
            : "It stops at its next step. What it has already done stays done: nothing is rolled back."
        }
        footer={
          <>
            <DialogClose asChild>
              <Button variant="secondary">Keep it running</Button>
            </DialogClose>
            <DialogClose asChild>
              <Button
                variant="danger"
                onClick={() =>
                  cancel.mutate(task.id, {
                    onSuccess: (after) =>
                      toast({
                        title:
                          after.status === "cancelled"
                            ? "Task cancelled"
                            : `The task had already ${TASK_STATUS_META[after.status].label.toLowerCase()}`,
                      }),
                    onError: () => toast({ title: "The task could not be cancelled" }),
                  })
                }
              >
                Cancel task
              </Button>
            </DialogClose>
          </>
        }
      />
    </Dialog>
  );
}
