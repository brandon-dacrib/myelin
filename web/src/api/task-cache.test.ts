import { describe, expect, it } from "vitest";
import { QueryClient } from "@tanstack/react-query";
import { applyAdminEvent, type AdminEvent } from "./events";
import { laterTask, rememberTask, taskIsBehind } from "./task-cache";
import type { Task } from "./tasks";

function task(overrides: Partial<Task>): Task {
  return {
    id: "01TASK",
    action: "user.redact_events",
    status: "running",
    resource: { type: "user", id: "@spammer:example.org" },
    created_at: "2026-09-28T21:57:38.152Z",
    started_at: "2026-09-28T21:57:38.152Z",
    finished_at: null,
    scheduled_for: null,
    progress: { current: 0, total: 2, unit: "events" },
    error: null,
    result: null,
    created_by: { kind: "user", id: "@ops:example.org" },
    ...overrides,
  } as Task;
}

const started = task({});
const halfway = task({ progress: { current: 1, total: 2, unit: "events" } });
const done = task({
  status: "succeeded",
  finished_at: "2026-09-28T21:57:38.154Z",
  progress: { current: 2, total: 2, unit: "events" },
  result: { total: 2, redacted: 2, failed_count: 0, failed: [] },
});

describe("taskIsBehind", () => {
  it("never lets an ended task run again, nor a running one lose progress", () => {
    expect(taskIsBehind(done, started)).toBe(true);
    expect(taskIsBehind(halfway, started)).toBe(true);
    expect(taskIsBehind(task({ status: "running" }), task({ status: "scheduled" }))).toBe(true);
    expect(taskIsBehind(started, halfway)).toBe(false);
    expect(taskIsBehind(started, done)).toBe(false);
    // Two ends (a cancel answered after the task succeeded): the newer sighting stands.
    expect(taskIsBehind(done, task({ status: "cancelled" }))).toBe(false);
  });

  it("keeps the later of two sightings", () => {
    expect(laterTask(undefined, started)).toBe(started);
    expect(laterTask(done, started)).toBe(done);
    expect(laterTask(started, done)).toBe(done);
  });
});

describe("a task that ends before the answer that started it arrives", () => {
  it("stays ended when the 202 is written after its last task.changed", () => {
    const qc = new QueryClient();
    // The server published the task's end on the event stream first...
    const event: AdminEvent = {
      id: "1",
      type: "task.changed",
      resource: { type: "task", id: done.id },
      data: done,
    };
    applyAdminEvent(qc, event);
    // ...and then the `202` with the task as it was when it started reached the page.
    rememberTask(qc, started);
    expect(qc.getQueryData<Task>(["task", done.id])?.status).toBe("succeeded");
  });
});
