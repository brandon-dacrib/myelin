import type { TaskStatus } from "@/api/tasks";
import { TASK_STATUSES } from "@/lib/tasks";

/** The Tasks list's filters as they live in the URL, named after `GET /tasks`'s parameters. */
export interface TasksSearch {
  status?: TaskStatus;
  action?: string;
  cursor?: string;
}

export function validateTasksSearch(search: Record<string, unknown>): TasksSearch {
  return {
    status: TASK_STATUSES.includes(search.status as TaskStatus)
      ? (search.status as TaskStatus)
      : undefined,
    action: typeof search.action === "string" && search.action ? search.action : undefined,
    cursor: typeof search.cursor === "string" && search.cursor ? search.cursor : undefined,
  };
}
