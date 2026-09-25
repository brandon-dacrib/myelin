import { useState } from "react";
import { ScrollText } from "lucide-react";
import { Link, useNavigate, useSearch } from "@tanstack/react-router";
import { downloadAuditExport, useAuditEntries } from "@/api/audit";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { ResourceLink } from "@/components/ResourceLink";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { Field, Input } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { hasScope } from "@/lib/auth";
import {
  describeAction,
  succeeded,
  TARGET_TYPES,
  targetTypeLabel,
  type AuditEntry,
} from "@/lib/audit";
import { hasAuditFilters, type AuditSearch } from "./audit-search";

/** Audit filters and pagination are URL state, including on the entry detail page. */
export function AuditPage() {
  const search = useSearch({ from: "/audit" });
  const navigate = useNavigate({ from: "/audit" });
  const query = useAuditEntries({ ...search, limit: 10 });
  const [exporting, setExporting] = useState(false);
  const [exportError, setExportError] = useState<unknown>();

  async function exportLog() {
    setExporting(true);
    setExportError(undefined);
    try {
      await downloadAuditExport(search);
    } catch (error) {
      setExportError(error);
    } finally {
      setExporting(false);
    }
  }

  const columns: Column<AuditEntry>[] = [
    { key: "when", header: "When", render: (entry) => <RelativeTime at={entry.recorded_at} /> },
    {
      key: "action",
      header: "Action",
      interactive: true,
      render: (entry) => (
        <Link
          to="/audit/$entryId"
          params={{ entryId: entry.id }}
          search={search}
          className="font-medium text-text hover:text-accent hover:underline"
        >
          {describeAction(entry.action)}
        </Link>
      ),
    },
    {
      key: "actor",
      header: "Who",
      priority: 2,
      render: (entry) => (
        <span className="break-all" title={entry.actor.id}>
          {entry.actor.display_name ?? entry.actor.id}
        </span>
      ),
    },
    {
      key: "target",
      header: "Resource",
      interactive: true,
      render: (entry) => <ResourceLink target={entry.target} className="break-all" />,
    },
    {
      key: "outcome",
      header: "Outcome",
      render: (entry) => (
        <Badge status={succeeded(entry) ? "success" : "danger"}>
          {succeeded(entry) ? "Succeeded" : "Failed"}
        </Badge>
      ),
    },
  ];

  if (!hasScope("admin:read"))
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Audit log</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );

  return (
    <div className="mx-auto max-w-[90rem] space-y-5 p-6">
      <div>
        <h1 className="text-xl text-text">Audit log</h1>
        <p className="mt-1 text-sm text-text-muted">
          Who changed what, when, and whether it succeeded. Newest first.
        </p>
      </div>
      <AuditFiltersForm
        key={JSON.stringify(search)}
        search={search}
        apply={(filters) => navigate({ search: filters })}
      />
      <div className="flex flex-wrap items-center gap-3">
        <Button variant="secondary" disabled={exporting} onClick={exportLog}>
          {exporting ? "Exporting…" : "Export NDJSON"}
        </Button>
        <p className="max-w-xl text-xs text-text-muted">
          Exports up to 10,000 entries in the applied date range. Actor, action, resource and
          outcome filters do not apply to the download.
        </p>
      </div>
      {exportError !== undefined && (
        <QueryProblemState
          error={exportError}
          resource="the audit export"
          compact
          onRetry={exportLog}
        />
      )}
      {query.isError ? (
        <QueryProblemState
          error={query.error}
          resource="the audit log"
          scope="admin:read"
          onRetry={() => query.refetch()}
        />
      ) : (
        <>
          <DataTable
            caption="Audit entries"
            columns={columns}
            rows={query.data?.items ?? []}
            getRowId={(entry) => entry.id}
            loading={query.isLoading}
            empty={
              <EmptyState
                icon={<ScrollText aria-hidden="true" />}
                title={
                  hasAuditFilters(search) ? "No matching audit entries" : "No changes recorded yet"
                }
                description={
                  hasAuditFilters(search)
                    ? "Try changing or clearing the filters."
                    : "Admin actions will appear here."
                }
              />
            }
          />
          <nav aria-label="Audit pagination" className="flex justify-between">
            <Button
              variant="ghost"
              disabled={!search.cursor || query.isFetching}
              onClick={() =>
                navigate({ search: { ...search, cursor: query.data?.prev_cursor ?? undefined } })
              }
            >
              Previous
            </Button>
            <Button
              variant="ghost"
              disabled={!query.data?.next_cursor || query.isFetching}
              onClick={() =>
                navigate({ search: { ...search, cursor: query.data?.next_cursor ?? undefined } })
              }
            >
              Next
            </Button>
          </nav>
        </>
      )}
    </div>
  );
}

function AuditFiltersForm({
  search,
  apply,
}: {
  search: AuditSearch;
  apply: (filters: AuditSearch) => void;
}) {
  const [draft, setDraft] = useState(search);
  const [error, setError] = useState("");
  const update = (key: keyof AuditSearch, value: string) =>
    setDraft({ ...draft, [key]: value || undefined });
  return (
    <form
      className="space-y-3 rounded-md border border-border bg-surface p-4"
      onSubmit={(event) => {
        event.preventDefault();
        if (
          draft.recorded_after &&
          draft.recorded_before &&
          draft.recorded_after >= draft.recorded_before
        ) {
          setError("The end of the date range must be after its start.");
          return;
        }
        setError("");
        apply({ ...draft, cursor: undefined });
      }}
    >
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <Field label="Actor ID">
          {(props) => (
            <Input
              {...props}
              placeholder="@ops:example.org"
              value={draft.actor ?? ""}
              onChange={(event) => update("actor", event.target.value)}
            />
          )}
        </Field>
        <Field label="Action">
          {(props) => (
            <Input
              {...props}
              placeholder="users.reset_password"
              value={draft.action ?? ""}
              onChange={(event) => update("action", event.target.value)}
            />
          )}
        </Field>
        <Field label="Resource type">
          {(props) => (
            <Select
              {...props}
              value={draft.target_type ?? "all"}
              onValueChange={(value) => update("target_type", value === "all" ? "" : value)}
              options={[
                { value: "all", label: "All resources" },
                ...TARGET_TYPES.map((value) => ({ value, label: targetTypeLabel(value) })),
              ]}
            />
          )}
        </Field>
        <Field label="Resource ID">
          {(props) => (
            <Input
              {...props}
              value={draft.target_id ?? ""}
              onChange={(event) => update("target_id", event.target.value)}
            />
          )}
        </Field>
        <Field label="Outcome">
          {(props) => (
            <Select
              {...props}
              value={draft.outcome ?? "all"}
              onValueChange={(value) => update("outcome", value === "all" ? "" : value)}
              options={[
                { value: "all", label: "All outcomes" },
                { value: "success", label: "Succeeded" },
                { value: "failure", label: "Failed" },
              ]}
            />
          )}
        </Field>
        <Field label="From (UTC)">
          {(props) => (
            <Input
              {...props}
              type="datetime-local"
              step="1"
              value={draft.recorded_after?.replace(/Z$/, "") ?? ""}
              onChange={(event) =>
                update(
                  "recorded_after",
                  event.target.value ? new Date(`${event.target.value}Z`).toISOString() : "",
                )
              }
            />
          )}
        </Field>
        <Field label="Before (UTC)">
          {(props) => (
            <Input
              {...props}
              type="datetime-local"
              step="1"
              value={draft.recorded_before?.replace(/Z$/, "") ?? ""}
              onChange={(event) =>
                update(
                  "recorded_before",
                  event.target.value ? new Date(`${event.target.value}Z`).toISOString() : "",
                )
              }
            />
          )}
        </Field>
      </div>
      {error && (
        <p role="alert" className="text-sm text-danger">
          {error}
        </p>
      )}
      <div className="flex gap-2">
        <Button type="submit">Apply filters</Button>
        <Button
          type="button"
          variant="ghost"
          onClick={() => {
            setDraft({});
            setError("");
            apply({});
          }}
        >
          Clear filters
        </Button>
      </div>
    </form>
  );
}
