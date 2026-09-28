import type { Task } from "@/api/tasks";
import { Badge } from "@/components/ui/badge/Badge";
import { describeProgress, progressFraction, TASK_STATUS_META } from "@/lib/tasks";
import { cn } from "@/lib/cn";

/** A task's status pill, and while it runs, how far it has got. */
export function TaskStatus({ task, className }: { task: Task; className?: string }) {
  const meta = TASK_STATUS_META[task.status];
  const fraction = progressFraction(task);
  const words = describeProgress(task);
  return (
    <div className={cn("flex min-w-32 flex-col gap-1", className)}>
      <Badge status={meta.status} className="self-start">
        {meta.label}
      </Badge>
      {task.status === "running" && (
        <TaskProgressBar fraction={fraction} label={words ?? "Working"} compact />
      )}
    </div>
  );
}

/**
 * A progress bar: a real `progressbar` with its numbers, or an indeterminate one when the task
 * has not said how much there is to do.
 */
export function TaskProgressBar({
  fraction,
  label,
  compact,
}: {
  fraction: number | null;
  label: string;
  compact?: boolean;
}) {
  return (
    <div className="flex flex-col gap-1">
      <div
        role="progressbar"
        aria-label={label}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={fraction == null ? undefined : Math.round(fraction * 100)}
        className={cn("overflow-hidden rounded-full bg-surface-sunken", compact ? "h-1.5" : "h-2")}
      >
        <div
          className={cn(
            "h-full rounded-full bg-accent transition-[width] duration-500",
            fraction == null && "w-1/3 animate-pulse",
          )}
          style={fraction == null ? undefined : { width: `${fraction * 100}%` }}
        />
      </div>
      <span className="text-xs text-text-muted tabular-nums">{label}</span>
    </div>
  );
}
