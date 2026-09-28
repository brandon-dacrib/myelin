import { Flag, X } from "lucide-react";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { useReports, type Report } from "@/api/reports";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { Field } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { hasScope } from "@/lib/auth";
import { REPORT_KIND_LABELS, REPORT_STATUS_META, reportSubject } from "@/lib/reports";
import { reportsQuery, type ReportsSearch } from "./reports-search";

/**
 * `/reports`: the moderation queue (information-architecture.md, Reports). Opens on the open
 * reports, newest first; the filters and the page cursor are URL state.
 */
export function ReportsPage() {
  const search = useSearch({ from: "/reports" });
  const navigate = useNavigate({ from: "/reports" });
  const query = useReports(reportsQuery(search));
  const status = search.status ?? "open";

  const setFilter = (patch: Partial<ReportsSearch>) =>
    navigate({ search: { ...search, ...patch, cursor: undefined } });

  const columns: Column<Report>[] = [
    {
      key: "subject",
      header: "Reported",
      interactive: true,
      render: (report) => (
        <Link
          to="/reports/$reportId"
          params={{ reportId: report.id }}
          search={search}
          className="font-medium text-text hover:text-accent hover:underline"
        >
          {reportSubject(report)}
        </Link>
      ),
    },
    {
      key: "reason",
      header: "Reason",
      render: (report) =>
        report.reason ? (
          <span className="line-clamp-2">{report.reason}</span>
        ) : (
          <span className="text-text-faint">No reason given</span>
        ),
    },
    {
      key: "reporter",
      header: "Reporter",
      priority: 2,
      render: (report) => <span className="break-all font-identifier">{report.reporter_id}</span>,
    },
    {
      key: "kind",
      header: "Kind",
      priority: 3,
      render: (report) => REPORT_KIND_LABELS[report.kind],
    },
    {
      key: "received",
      header: "Received",
      render: (report) => <RelativeTime at={report.received_at} />,
    },
    {
      key: "status",
      header: "Status",
      render: (report) => (
        <Badge status={REPORT_STATUS_META[report.status].status}>
          {REPORT_STATUS_META[report.status].label}
        </Badge>
      ),
    },
  ];

  if (!hasScope("moderation:read"))
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Reports</h1>
        <ForbiddenState scope="moderation:read" />
      </div>
    );

  const filtered = status !== "open" || search.kind || search.room_id;

  return (
    <div className="mx-auto max-w-[90rem] space-y-5 p-6">
      <div>
        <h1 className="text-xl text-text">Reports</h1>
        <p className="mt-1 text-sm text-text-muted">
          What people on this server have flagged — a message, a room or a person — and what was
          done about it.
        </p>
      </div>

      <div className="grid gap-3 rounded-md border border-border bg-surface p-4 sm:grid-cols-3">
        <Field label="Status">
          {(props) => (
            <Select
              {...props}
              value={status}
              onValueChange={(value) => setFilter({ status: value as ReportsSearch["status"] })}
              options={[
                { value: "open", label: "Open" },
                { value: "resolved", label: "Resolved" },
                { value: "dismissed", label: "Dismissed" },
                { value: "all", label: "All reports" },
              ]}
            />
          )}
        </Field>
        <Field label="Kind">
          {(props) => (
            <Select
              {...props}
              value={search.kind ?? "all"}
              onValueChange={(value) =>
                setFilter({ kind: value === "all" ? undefined : (value as ReportsSearch["kind"]) })
              }
              options={[
                { value: "all", label: "Every kind" },
                { value: "event", label: "Messages" },
                { value: "room", label: "Rooms" },
                { value: "user", label: "Users" },
              ]}
            />
          )}
        </Field>
        <Field label="Order">
          {(props) => (
            <Select
              {...props}
              value={search.sort ?? "-received_at"}
              onValueChange={(value) =>
                setFilter({
                  sort: value === "-received_at" ? undefined : (value as ReportsSearch["sort"]),
                })
              }
              options={[
                { value: "-received_at", label: "Newest first" },
                { value: "received_at", label: "Oldest first" },
                { value: "score", label: "Most offensive first" },
              ]}
            />
          )}
        </Field>
        {search.room_id && (
          <div className="flex items-center gap-2 text-sm text-text sm:col-span-3">
            <span className="text-text-muted">Only reports about</span>
            <span className="break-all font-identifier">{search.room_id}</span>
            <Button
              variant="ghost"
              size="sm"
              leadingIcon={<X size={14} aria-hidden="true" />}
              onClick={() => setFilter({ room_id: undefined })}
            >
              Every room
            </Button>
          </div>
        )}
      </div>

      {query.isError ? (
        <QueryProblemState
          error={query.error}
          resource="reports"
          scope="moderation:read"
          onRetry={() => query.refetch()}
        />
      ) : (
        <DataTable
          caption="Reports"
          columns={columns}
          rows={query.data?.items ?? []}
          getRowId={(report) => report.id}
          loading={query.isLoading}
          onRowClick={(report) =>
            navigate({ to: "/reports/$reportId", params: { reportId: report.id }, search })
          }
          empty={
            <EmptyState
              icon={<Flag aria-hidden="true" />}
              variant={filtered ? "filtered" : "page"}
              title={filtered ? "No matching reports" : "No open reports"}
              description={
                filtered
                  ? "Try another status or kind."
                  : "Nothing is waiting for a decision. When somebody reports a message, a room or a person, it appears here."
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
