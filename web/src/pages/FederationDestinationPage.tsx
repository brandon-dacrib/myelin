import type { ReactNode } from "react";
import { useParams, Link } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { useFederationDestination, useResetFederationDestination } from "@/api/federation";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { ErrorState, ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { CopyableId } from "@/components/CopyableId";
import { RelativeTime } from "@/components/RelativeTime";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";

/** `/federation/:serverName` — flows.md flow 4 step 3. */
export function FederationDestinationPage() {
  const { serverName } = useParams({ from: "/federation/$serverName" });
  const { data: destination, isLoading, isError, refetch } = useFederationDestination(serverName);
  const canWrite = hasScope("admin:write");
  const reset = useResetFederationDestination();

  if (!hasScope("admin:read")) {
    return (
      <div className="p-6">
        <ForbiddenState scope="admin:read" />
      </div>
    );
  }

  if (isLoading) {
    return (
      <div className="p-6">
        <SkeletonText lines={4} />
      </div>
    );
  }

  if (isError || !destination) {
    return (
      <div className="p-6">
        <ErrorState title="Couldn't load this destination" onRetry={() => refetch()} />
      </div>
    );
  }

  const status = destination.failing_since
    ? "danger"
    : destination.retry_interval_ms
      ? "warning"
      : "success";
  const label = destination.failing_since
    ? "Failing"
    : destination.retry_interval_ms
      ? "Backing off"
      : "Healthy";

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <Link
        to="/federation"
        className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
      >
        <ChevronLeft size={14} aria-hidden="true" />
        Federation
      </Link>

      <div className="mt-2 flex flex-wrap items-start justify-between gap-4">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="font-identifier text-xl text-text">{destination.server_name}</h1>
            <Badge status={status}>{label}</Badge>
          </div>
          <p className="mt-1 text-sm text-text-muted">
            <CopyableId value={destination.server_name ?? ""} />
          </p>
        </div>
        <Button
          variant="secondary"
          disabled={!canWrite}
          title={!canWrite ? "Needs admin:write" : undefined}
          onClick={() =>
            reset.mutate(destination.server_name ?? "", {
              onSuccess: () => toast({ title: `Backoff reset for ${destination.server_name}` }),
              onError: () => toast({ title: "Couldn't reset backoff", variant: "danger" }),
            })
          }
        >
          Reset backoff
        </Button>
      </div>

      <dl className="mt-6 grid grid-cols-1 gap-x-8 gap-y-4 sm:grid-cols-2 xl:grid-cols-3">
        <Fact label="Last success" value={<RelativeTime at={destination.last_successful_at} />} />
        <Fact label="Failing since" value={<RelativeTime at={destination.failing_since} />} />
        <Fact label="Next retry" value={<RelativeTime at={destination.retry_last_at} />} />
        <Fact
          label="Retry interval"
          value={
            destination.retry_interval_ms != null
              ? `${Math.round(destination.retry_interval_ms / 1000)} s`
              : "—"
          }
        />
        <Fact label="Pending PDUs" value={String(destination.pending_pdu_count ?? 0)} />
        <Fact label="Pending EDUs" value={String(destination.pending_edu_count ?? 0)} />
      </dl>
    </div>
  );
}

function Fact({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div>
      <dt className="text-xs text-text-muted">{label}</dt>
      <dd className="mt-0.5 text-sm text-text">{value}</dd>
    </div>
  );
}
