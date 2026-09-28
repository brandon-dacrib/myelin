/**
 * Putting a task into the query cache without going back in time.
 *
 * A task reaches the cache three ways: the answer to the request that started it (a `202` with
 * the task as it was a moment after it started), a `task.changed` event on the admin event stream
 * (`./events`), and a fetch of `GET /tasks/{id}`. They can arrive in any order. A task that ends
 * within milliseconds (a user with two messages to redact) publishes its last `task.changed`
 * before the `202` that started it has reached the browser; writing that `202` over the cache
 * then showed the task running forever, because while the stream is connected nothing polls it
 * and no later event comes. So every write keeps whichever of the two is further along.
 */
import type { QueryClient } from "@tanstack/react-query";
import type { components } from "./schema";

type Task = components["schemas"]["Task"];

/** How far along a status is: scheduled, then running, then ended (any way). */
function stage(status: Task["status"]): number {
  if (status === "scheduled") return 0;
  if (status === "running") return 1;
  return 2;
}

/**
 * Whether `next` is behind `current`, two sightings of the same task: an ended task never runs
 * again, a running one is never scheduled again, and a running task's progress only grows.
 */
export function taskIsBehind(current: Task, next: Task): boolean {
  const [was, now] = [stage(current.status), stage(next.status)];
  if (now !== was) return now < was;
  if (now === 1) return (next.progress?.current ?? 0) < (current.progress?.current ?? 0);
  return false;
}

/** The later of two sightings of a task (the newer one when neither is behind). */
export function laterTask(current: Task | undefined, next: Task): Task {
  return current && current.id === next.id && taskIsBehind(current, next) ? current : next;
}

/** Puts `task` into its own query (`["task", id]`) unless the cache already holds a later state. */
export function rememberTask(qc: QueryClient, task: Task): void {
  qc.setQueryData<Task>(["task", task.id], (current) => laterTask(current, task));
}
