/**
 * Tasks (`GET /tasks`, `GET /tasks/{id}`, `POST /tasks/{id}/cancel`): long-running work the
 * server does on an operator's behalf (a bridge's backlog replay, a content-scan sweep, later a
 * room purge), kept by `crates/hs-admin/src/tasks.rs` and durable across restarts
 * (`crates/hs-cli/src/tasks.rs`). Reads need `admin:read`, cancel `admin:write`.
 *
 * Cancel is best effort: a task running on another replica is recorded `cancelled` at once and
 * stops at its next progress report, and cancelling one that has already ended changes nothing
 * and is not an error.
 */
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { api, newIdempotencyKey } from "./client";
import { unwrap } from "./problem";
import type { components, operations } from "./schema";
import { hasScope } from "@/lib/auth";

export type Task = components["schemas"]["Task"];
export type TaskStatus = Task["status"];

type TaskListQuery = NonNullable<operations["tasks.list"]["parameters"]["query"]>;

export interface TaskFilters {
  status?: TaskListQuery["status"];
  /** An action (`media.purge_remote_cache`) or a prefix ending in a dot (`media.`). */
  action?: string;
  cursor?: string;
  limit?: number;
}

/** Whether a task can still change: the pages poll faster while one is. */
export function taskIsActive(task: Pick<Task, "status">): boolean {
  return task.status === "running" || task.status === "scheduled";
}

export function useTasks(filters: TaskFilters, options?: { enabled?: boolean }) {
  return useQuery({
    queryKey: ["tasks", filters],
    enabled: hasScope("admin:read") && (options?.enabled ?? true),
    queryFn: async () => {
      const result = await api.GET("/tasks", { params: { query: filters } });
      return unwrap(result);
    },
    refetchInterval: (query) => (query.state.data?.items.some(taskIsActive) ? 3_000 : 30_000),
  });
}

export function useTask(id: string) {
  return useQuery({
    queryKey: ["task", id],
    enabled: hasScope("admin:read"),
    queryFn: async () => {
      const result = await api.GET("/tasks/{id}", { params: { path: { id } } });
      return unwrap(result);
    },
    refetchInterval: (query) =>
      query.state.data && taskIsActive(query.state.data) ? 2_000 : false,
  });
}

export function useCancelTask() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: async (id: string) => {
      const result = await api.POST("/tasks/{id}/cancel", {
        params: { path: { id }, header: { "Idempotency-Key": newIdempotencyKey() } },
      });
      return unwrap(result);
    },
    onSuccess: (task, id) => {
      qc.setQueryData(["task", id], task);
      qc.invalidateQueries({ queryKey: ["tasks"] });
    },
  });
}
