import { useMemo } from "react";
import { useNavigate, Link } from "@tanstack/react-router";
import { Globe } from "lucide-react";
import { useFederationDestinations } from "@/api/dashboard";
import type { Destination } from "@/api/federation";
import { Badge } from "@/components/ui/badge/Badge";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ErrorState, ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { RelativeTime } from "@/components/RelativeTime";
import { hasScope } from "@/lib/auth";

function destinationStatus(d: Destination): {
  status: "success" | "warning" | "danger";
  label: string;
} {
  if (d.failing_since) return { status: "danger", label: "Failing" };
  if (d.retry_interval_ms) return { status: "warning", label: "Backing off" };
  return { status: "success", label: "Healthy" };
}

/** `/federation` — flows.md flow 4: watch federation health. */
export function FederationPage() {
  const navigate = useNavigate();
  const canRead = hasScope("admin:read");
  const { data, isLoading, isError, refetch } = useFederationDestinations(50);

  const rows = useMemo(() => {
    const items = data?.items ?? [];
    // Attention-first: failing, then backing off, then healthy.
    const severity = { danger: 0, warning: 1, success: 2 } as const;
    return [...items].sort(
      (a, b) => severity[destinationStatus(a).status] - severity[destinationStatus(b).status],
    );
  }, [data]);

  const columns: Column<Destination>[] = [
    {
      key: "server_name",
      header: "Server",
      priority: 1,
      interactive: true,
      render: (d) => (
        <Link
          to="/federation/$serverName"
          params={{ serverName: d.server_name ?? "" }}
          className="font-identifier font-medium text-text hover:text-accent hover:underline"
        >
          {d.server_name}
        </Link>
      ),
    },
    {
      key: "status",
      header: "Status",
      priority: 1,
      render: (d) => {
        const meta = destinationStatus(d);
        return <Badge status={meta.status}>{meta.label}</Badge>;
      },
      renderCompact: (d) => destinationStatus(d).label,
    },
    {
      key: "last_successful_at",
      header: "Last success",
      priority: 2,
      render: (d) => <RelativeTime at={d.last_successful_at} />,
    },
    {
      key: "pending",
      header: "Pending",
      priority: 3,
      align: "end",
      render: (d) => (d.pending_pdu_count ?? 0) + (d.pending_edu_count ?? 0),
    },
  ];

  if (!canRead) {
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Federation</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <h1 className="text-xl text-text">Federation</h1>

      {isError && (
        <div className="mt-6">
          <ErrorState title="Couldn't load federation destinations" onRetry={() => refetch()} />
        </div>
      )}

      {!isError && (
        <div className="mt-4">
          <DataTable
            caption="Federation destinations"
            columns={columns}
            rows={rows}
            getRowId={(d) => d.server_name ?? ""}
            loading={isLoading}
            onRowClick={(d) =>
              navigate({
                to: "/federation/$serverName",
                params: { serverName: d.server_name ?? "" },
              })
            }
            empty={
              <EmptyState
                icon={<Globe aria-hidden="true" />}
                title="No federation traffic yet"
                description="When your users join rooms on other servers, those servers appear here."
              />
            }
          />
        </div>
      )}
    </div>
  );
}
