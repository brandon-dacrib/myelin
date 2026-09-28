import type { components } from "@/api/schema";
import { publishMockEvent, registerMockTicker } from "./events";

type Task = components["schemas"]["Task"];

/**
 * The mock's tasks, shaped like `crates/hs-admin/src/tasks.rs`'s wire type. One of them is
 * running and moves on a clock (a remote-media purge that finishes about twenty seconds after the
 * mock starts), so the pages' polling can be seen doing its job; the rest are settled.
 */

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const RUN_MS = 20_000;

interface MockTask extends Task {
  /** When the clock-driven task started, for {@link advance}. */
  clockStart?: number;
  /** Moves a task another part of the mock drives on, each time it is read (see {@link putDrivenTask}). */
  step?: (task: Task) => void;
}

const admin = { kind: "user" as const, id: "@admin:example.org", display_name: "Operator" };
const system = { kind: "system" as const, id: "hs" };

function seed(): MockTask[] {
  const now = Date.now();
  const iso = (msAgo: number) => new Date(now - msAgo).toISOString();
  return [
    {
      id: "01J9ZT000000000000000000T6",
      action: "media.purge_remote_cache",
      status: "running",
      resource: { type: "media", id: "remote-cache" },
      created_by: admin,
      created_at: iso(2 * MINUTE),
      started_at: iso(2 * MINUTE),
      finished_at: null,
      scheduled_for: null,
      progress: { current: 1200, total: 4000, unit: "files", message: "Deleting cached files" },
      error: null,
      result: null,
      clockStart: now,
    },
    {
      id: "01J9ZT000000000000000000T5",
      action: "room.purge_history",
      status: "scheduled",
      resource: { type: "room", id: "!spam-central:example.org" },
      created_by: admin,
      created_at: iso(10 * MINUTE),
      started_at: null,
      finished_at: null,
      scheduled_for: new Date(now + 2 * HOUR).toISOString(),
      progress: null,
      error: null,
      result: null,
    },
    {
      id: "01J9ZT000000000000000000T4",
      action: "room.delete",
      status: "failed",
      resource: { type: "room", id: "!whatsapp-portal-1:example.org" },
      created_by: admin,
      created_at: iso(3 * HOUR),
      started_at: iso(3 * HOUR),
      finished_at: iso(3 * HOUR - 40_000),
      scheduled_for: null,
      progress: { current: 3, total: 7, unit: "members" },
      error: {
        type: "urn:hs:problem:internal",
        title: "Internal error",
        status: 500,
        detail: "the room's owner replica stopped answering while members were being removed",
      },
      result: null,
    },
    {
      id: "01J9ZT000000000000000000T3",
      action: "appservice.replay",
      status: "succeeded",
      resource: { type: "appservice", id: "whatsapp" },
      created_by: admin,
      created_at: iso(26 * HOUR),
      started_at: iso(26 * HOUR),
      finished_at: iso(26 * HOUR - 12_000),
      scheduled_for: null,
      progress: { current: 42, total: 42, unit: "transactions" },
      error: null,
      result: { replayed: 42, failed: 0 },
    },
    {
      id: "01J9ZT000000000000000000T2",
      action: "media.delete",
      status: "cancelled",
      resource: { type: "user", id: "@spammer42:example.org" },
      created_by: admin,
      created_at: iso(3 * 24 * HOUR),
      started_at: iso(3 * 24 * HOUR),
      finished_at: iso(3 * 24 * HOUR - 5 * MINUTE),
      scheduled_for: null,
      progress: { current: 18, total: 60, unit: "files" },
      error: null,
      result: null,
    },
    {
      id: "01J9ZT000000000000000000T1",
      action: "media.resume_scans",
      status: "succeeded",
      resource: { type: "media", id: "scans" },
      created_by: system,
      created_at: iso(4 * 24 * HOUR),
      started_at: iso(4 * 24 * HOUR),
      finished_at: iso(4 * 24 * HOUR - 3_000),
      scheduled_for: null,
      progress: null,
      error: null,
      result: { rescanned: 3 },
    },
  ];
}

let tasks = seed();

/** Puts the tasks back as they were (Vitest runs this after every test). */
export function resetTasks(): void {
  tasks = seed();
}

/**
 * Records a task that has already ended, as the server does for an appservice replay
 * (`TaskRegistry::record_finished`), so the `Location` it answered with leads somewhere.
 */
export function recordFinishedTask(
  task: Omit<Task, "created_by"> & Partial<Pick<Task, "created_by">>,
): Task {
  const recorded: MockTask = { created_by: admin, ...task };
  tasks = [...tasks.filter((t) => t.id !== recorded.id), recorded];
  return changed(wire(recorded));
}

// A running clock-driven or step-driven task reports its progress on the event stream once a
// second, as a server task does each time it records progress. A step-driven task (a user's
// redaction) moves on only when it is read, and a page does not poll while the stream is
// connected, so the ticker is what reads it: without this it sat at its first step forever.
registerMockTicker(() => {
  for (const task of tasks) {
    const live = task.status === "running" || task.status === "scheduled";
    if (live && (task.clockStart !== undefined || task.step !== undefined)) changed(wire(task));
  }
});

/** Publishes a task's change on the mock event stream, as the server's registry does. */
function changed(task: Task): Task {
  publishMockEvent("task.changed", task, { type: "task", id: task.id });
  return task;
}

/**
 * Records or replaces a task another part of the mock drives itself (a replica's drain, which
 * `./cluster` moves on and finishes as its shards are handed off), so it is on the Tasks page
 * exactly as the server's own task would be.
 */
export function putTask(task: Omit<Task, "created_by"> & Partial<Pick<Task, "created_by">>): Task {
  return recordFinishedTask(task);
}

/**
 * Records a running task that `step` moves on each time it is read, so a page polling it sees
 * it progress and finish without the mock needing a timer (a user's redaction does this).
 */
export function putDrivenTask(
  task: Omit<Task, "created_by"> & Partial<Pick<Task, "created_by">>,
  step: (task: Task) => void,
): Task {
  const recorded: MockTask = { created_by: admin, ...task, step };
  tasks = [...tasks.filter((t) => t.id !== recorded.id), recorded];
  return changed(wire(recorded));
}

/** Moves the clock-driven task on:its progress follows the clock and it succeeds at the end. */
function advance(task: MockTask): MockTask {
  if (task.step && (task.status === "running" || task.status === "scheduled")) {
    task.step(task);
    return task;
  }
  if (task.clockStart === undefined || task.status !== "running") return task;
  const elapsed = Date.now() - task.clockStart;
  const total = task.progress?.total ?? 4000;
  const start = 1200;
  if (elapsed >= RUN_MS) {
    task.status = "succeeded";
    task.finished_at = new Date().toISOString();
    task.progress = { current: total, total, unit: "files" };
    task.result = { deleted_files: total, freed_bytes: 3_221_225_472 };
  } else {
    const current = Math.floor(start + ((total - start) * elapsed) / RUN_MS);
    task.progress = { ...task.progress, current, total };
  }
  return task;
}

function wire(task: MockTask): Task {
  const { clockStart: _clockStart, step: _step, ...rest } = advance(task);
  return rest;
}

export function listTasks(query: URLSearchParams): Task[] {
  const status = query.get("status");
  const action = query.get("action");
  return tasks
    .map(wire)
    .filter(
      (t) =>
        (!status || t.status === status) &&
        (!action || (action.endsWith(".") ? t.action.startsWith(action) : t.action === action)),
    )
    .sort((a, b) => b.id.localeCompare(a.id));
}

export function getTask(id: string): Task | undefined {
  const task = tasks.find((t) => t.id === id);
  return task && wire(task);
}

/** Best-effort cancel, as the server does it: an ended task is answered as it is. */
export function cancelTask(id: string): Task | undefined {
  const task = tasks.find((t) => t.id === id);
  if (!task) return undefined;
  advance(task);
  if (task.status === "running" || task.status === "scheduled") {
    task.status = "cancelled";
    task.finished_at = new Date().toISOString();
    return changed(wire(task));
  }
  return wire(task);
}
