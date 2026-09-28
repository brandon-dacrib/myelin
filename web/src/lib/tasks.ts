import type { Task, TaskStatus } from "@/api/tasks";

/** How the interface words a task's state and what it is doing. */

type BadgeStatus = "success" | "warning" | "danger" | "info" | "muted" | "neutral";

export const TASK_STATUSES: readonly TaskStatus[] = [
  "running",
  "scheduled",
  "succeeded",
  "failed",
  "cancelled",
];

export const TASK_STATUS_META: Record<TaskStatus, { label: string; status: BadgeStatus }> = {
  scheduled: { label: "Scheduled", status: "neutral" },
  running: { label: "Running", status: "info" },
  succeeded: { label: "Succeeded", status: "success" },
  failed: { label: "Failed", status: "danger" },
  cancelled: { label: "Cancelled", status: "muted" },
};

/**
 * The task actions the contract names (`Task.action` is an open enum), as an operator would
 * read them. An unknown one is spelled out from its name rather than hidden.
 */
const TASK_ACTIONS: Record<string, string> = {
  "room.delete": "Delete room",
  "room.purge_history": "Purge room history",
  "user.redact_events": "Redact a user's messages",
  "user.deactivate": "Deactivate user",
  "media.delete": "Delete media",
  "media.purge_remote_cache": "Purge remote media cache",
  "media.resume_scans": "Resume content scans",
  "appservice.replay": "Replay bridge transactions",
  "migration.copy": "Copy data from Synapse",
  "migration.verify": "Verify the migration",
  "migration.cutover": "Cut over from Synapse",
  "federation.refetch_keys": "Refetch server keys",
};

export function describeTaskAction(action: string): string {
  if (TASK_ACTIONS[action]) return TASK_ACTIONS[action];
  const words = action.replace(/[._]/g, " ").trim();
  return words.charAt(0).toUpperCase() + words.slice(1);
}

/** Progress as a fraction in [0, 1], or `null` when the task has not said how far it has to go. */
export function progressFraction(task: Pick<Task, "progress">): number | null {
  const { current, total } = task.progress ?? {};
  if (current == null || !total) return null;
  return Math.min(Math.max(current / total, 0), 1);
}

/** "1,200 of 4,000 files", "1,200 files", or `null` when there is nothing to say. */
export function describeProgress(task: Pick<Task, "progress">): string | null {
  const progress = task.progress;
  if (!progress || progress.current == null) return null;
  const unit = progress.unit ? ` ${progress.unit}` : "";
  return progress.total
    ? `${progress.current.toLocaleString()} of ${progress.total.toLocaleString()}${unit}`
    : `${progress.current.toLocaleString()}${unit}`;
}
