import type { ReactNode } from "react";
import { useParams, Link } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { useFederationDestination, useResetFederationDestination } from "@/api/federation";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { QueryProblemState } from "@/components/QueryProblemState";
import { CopyableId } from "@/components/CopyableId";
import { RelativeTime } from "@/components/RelativeTime";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import { destinationHealth, formatInterval, nextAttemptAt } from "@/lib/federation";
import { CatchUpNotice } from "./federation/CatchUp";
import { DestinationRoomsPanel, RemoteKeysPanel } from "./federation/FederationPanels";

/** `/federation/:serverName` — flows.md flow 4 step 3. */
export function FederationDestinationPage() {
  const { serverName } = useParams({ from: "/federation/$serverName" });
  const {
    data: destination,
    isLoading,
    isError,
    error,
    refetch,
  } = useFederationDestination(serverName);
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
        <QueryProblemState error={error} resource="this destination" onRetry={() => refetch()} />
      </div>
    );
  }

  const health = destinationHealth(destination);
  const nextAttempt = nextAttemptAt(destination);
  const catchingUp = Boolean(destination.catch_up_since);

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
            <Badge status={health.status}>{health.label}</Badge>
            {catchingUp && (
              <Badge status="info" hideIcon>
                Catching up
              </Badge>
            )}
          </div>
          <p className="mt-1 text-sm text-text-muted">
            <CopyableId value={destination.server_name ?? ""} />
          </p>
          <p className="mt-2 max-w-2xl text-sm text-text-muted">{health.explanation}</p>
        </div>
        <div className="flex max-w-xs flex-col items-end gap-1">
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
          <p className="text-right text-xs text-text-muted">
            Forgets the failures and tries this server at once, instead of waiting out the backoff.
            Use it when you know the server is back.
          </p>
        </div>
      </div>

      {destination.catch_up_since && <CatchUpNotice since={destination.catch_up_since} />}

      <dl className="mt-6 grid grid-cols-1 gap-x-8 gap-y-4 sm:grid-cols-2 xl:grid-cols-3">
        <Fact
          label="Last success"
          value={<RelativeTime at={destination.last_successful_at} />}
          hint="When this server last accepted something sent to it."
        />
        <Fact
          label="Failing since"
          value={
            destination.failing_since ? (
              <RelativeTime at={destination.failing_since} />
            ) : (
              "Not failing"
            )
          }
          hint="When the current run of failed attempts began."
        />
        <Fact
          label="Last attempt"
          value={<RelativeTime at={destination.retry_last_at} />}
          hint="When this server last tried to reach it."
        />
        <Fact
          label="Next attempt"
          value={
            !nextAttempt ? (
              "As soon as there is something to send"
            ) : nextAttempt.due ? (
              "Due now, with the next thing to send"
            ) : (
              <RelativeTime at={nextAttempt.at} />
            )
          }
          hint="After a failure, the wait grows with each further failure, up to the setting Max retry backoff."
        />
        <Fact
          label="Wait between attempts"
          value={
            destination.retry_interval_ms != null
              ? formatInterval(destination.retry_interval_ms)
              : "None, it is not backing off"
          }
          hint="The current backoff. Reset backoff sets it to nothing."
        />
        <Fact
          label="Events waiting (PDUs)"
          value={
            catchingUp ? "Not queued while catching up" : String(destination.pending_pdu_count ?? 0)
          }
          hint="Room events queued for this server, sent in order as soon as it answers."
        />
        <Fact
          label="Other messages waiting (EDUs)"
          value={String(destination.pending_edu_count ?? 0)}
          hint="Typing notices, read receipts, presence and device-list updates queued for it."
        />
      </dl>

      <DestinationRoomsPanel serverName={destination.server_name ?? serverName} />
      <RemoteKeysPanel serverName={destination.server_name ?? serverName} />
    </div>
  );
}

function Fact({ label, value, hint }: { label: string; value: ReactNode; hint?: string }) {
  return (
    <div>
      <dt className="text-xs text-text-muted">{label}</dt>
      <dd className="mt-0.5 text-sm text-text">{value}</dd>
      {hint && <dd className="mt-0.5 text-xs text-text-faint">{hint}</dd>}
    </div>
  );
}
