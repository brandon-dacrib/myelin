import { ListChecks } from "lucide-react";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { useTasks, type Task } from "@/api/tasks";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { ResourceLink } from "@/components/ResourceLink";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { Field } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { hasScope } from "@/lib/auth";
import { describeTaskAction, TASK_STATUS_META, TASK_STATUSES } from "@/lib/tasks";
import { TaskStatus } from "./TaskProgress";
import type { TasksSearch } from "./tasks-search";

/** The action families `GET /tasks?action=` takes as a prefix ending in a dot. */
const ACTION_FAMILIES = [
  { value: "room.", label: "Rooms" },
  { value: "user.", label: "Users" },
  { value: "media.", label: "Media" },
  { value: "appservice.", label: "Bridges" },
  { value: "federation.", label: "Federation" },
  { value: "migration.", label: "Migration" },
];

/**
 * `/tasks`: long-running work the server is doing or did for an administrator, newest first.
 * Polls every few seconds while anything on the page is running, so progress moves on its own.
 */
export function TasksPage() {
  const search = useSearch({ from: "/tasks" });
  const navigate = useNavigate({ from: "/tasks" });
  const query = useTasks({ ...search, limit: 25 });

  const setFilter = (patch: Partial<TasksSearch>) =>
    navigate({ search: { ...search, ...patch, cursor: undefined } });

  const columns: Column<Task>[] = [
    {
      key: "action",
      header: "Task",
      interactive: true,
      render: (task) => (
        <Link
          to="/tasks/$taskId"
          params={{ taskId: task.id }}
          search={search}
          className="font-medium text-text hover:text-accent hover:underline"
        >
          {describeTaskAction(task.action)}
        </Link>
      ),
    },
    {
      key: "resource",
      header: "On",
      interactive: true,
      priority: 2,
      render: (task) =>
        task.resource ? (
          <ResourceLink target={task.resource} className="break-all" />
        ) : (
          <span className="text-text-faint">—</span>
        ),
    },
    { key: "status", header: "Status", render: (task) => <TaskStatus task={task} /> },
    {
      key: "by",
      header: "Started by",
      priority: 3,
      render: (task) =>
        task.created_by?.kind === "system" ? (
          <span className="text-text-muted">The server</span>
        ) : (
          <span className="break-all">
            {task.created_by?.display_name ?? task.created_by?.id ?? "—"}
          </span>
        ),
    },
    { key: "created", header: "Created", render: (task) => <RelativeTime at={task.created_at} /> },
  ];

  if (!hasScope("admin:read"))
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Tasks</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );

  const filtered = Boolean(search.status || search.action);

  return (
    <div className="mx-auto max-w-[90rem] space-y-5 p-6">
      <div>
        <h1 className="text-xl text-text">Tasks</h1>
        <p className="mt-1 text-sm text-text-muted">
          Work that takes longer than a request: purges, deletions, bridge replays and scans. Newest
          first; they are kept for 30 days after they finish.
        </p>
      </div>
      <div className="grid gap-3 rounded-md border border-border bg-surface p-4 sm:grid-cols-2 lg:grid-cols-4">
        <Field label="Status">
          {(props) => (
            <Select
              {...props}
              value={search.status ?? "all"}
              onValueChange={(value) =>
                setFilter({ status: value === "all" ? undefined : (value as Task["status"]) })
              }
              options={[
                { value: "all", label: "Any status" },
                ...TASK_STATUSES.map((s) => ({ value: s, label: TASK_STATUS_META[s].label })),
              ]}
            />
          )}
        </Field>
        <Field label="Kind">
          {(props) => (
            <Select
              {...props}
              value={search.action ?? "all"}
              onValueChange={(value) => setFilter({ action: value === "all" ? undefined : value })}
              options={[
                { value: "all", label: "Every kind" },
                ...ACTION_FAMILIES,
                // An exact action from a shared link, so the control can show it.
                ...(search.action && !ACTION_FAMILIES.some((f) => f.value === search.action)
                  ? [{ value: search.action, label: describeTaskAction(search.action) }]
                  : []),
              ]}
            />
          )}
        </Field>
      </div>
      {query.isError ? (
        <QueryProblemState
          error={query.error}
          resource="tasks"
          scope="admin:read"
          onRetry={() => query.refetch()}
        />
      ) : (
        <DataTable
          caption="Tasks"
          columns={columns}
          rows={query.data?.items ?? []}
          getRowId={(task) => task.id}
          loading={query.isLoading}
          onRowClick={(task) =>
            navigate({ to: "/tasks/$taskId", params: { taskId: task.id }, search })
          }
          empty={
            <EmptyState
              icon={<ListChecks aria-hidden="true" />}
              variant={filtered ? "filtered" : "page"}
              title={filtered ? "No matching tasks" : "No tasks yet"}
              description={
                filtered
                  ? "Try another status or kind."
                  : "When the server does something that takes a while, such as replaying a bridge's backlog, it appears here with its progress."
              }
            />
          }
          pagination={{
            hasPrevious: Boolean(search.cursor) && !query.isFetching,
            hasNext: Boolean(query.data?.next_cursor) && !query.isFetching,
            onPrevious: () =>
              navigate({ search: { ...search, cursor: query.data?.prev_cursor ?? undefined } }),
            onNext: () =>
              navigate({ search: { ...search, cursor: query.data?.next_cursor ?? undefined } }),
          }}
        />
      )}
    </div>
  );
}
