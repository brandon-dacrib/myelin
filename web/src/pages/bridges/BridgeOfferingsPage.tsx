import { useMemo } from "react";
import { Link, useNavigate } from "@tanstack/react-router";
import { Cable } from "lucide-react";
import {
  useBridgeDeploymentTarget,
  useBridgeOfferings,
  useBridgeTypes,
  type BridgeOffering,
} from "@/api/bridges";
import { BridgeGlyph } from "@/components/BridgeGlyph";
import { CopyableId } from "@/components/CopyableId";
import { QueryProblemState } from "@/components/QueryProblemState";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { hasScope } from "@/lib/auth";
import {
  instanceCountList,
  instanceStateBadge,
  instanceStateLabel,
  runtimeMeta,
  totalInstances,
} from "@/lib/bridge-offerings";
import { BridgesTabs } from "./BridgesTabs";

/**
 * `/bridges` -- the bridges this server offers (RFC 0017): which networks people here can
 * connect, where each person's bridge runs, the address people message to get one, and how
 * their bridges are doing. Each person's bridge is also an appservice registration; those are
 * one tab over.
 */
export function BridgeOfferingsPage() {
  const navigate = useNavigate();
  const canRead = hasScope("bridges:read");
  const canWrite = hasScope("bridges:write");
  const { data, isLoading, isError, error, refetch } = useBridgeOfferings();
  const { data: types } = useBridgeTypes();
  const { data: target } = useBridgeDeploymentTarget();

  const rows = useMemo(
    () => [...(data ?? [])].sort((a, b) => (a.name ?? a.type).localeCompare(b.name ?? b.type)),
    [data],
  );

  const columns: Column<BridgeOffering>[] = [
    {
      key: "name",
      header: "Bridge",
      priority: 1,
      interactive: true,
      render: (o) => {
        const type = types?.find((t) => t.id === o.type);
        return (
          <span className="flex items-center gap-3">
            <BridgeGlyph category={type?.category} size="sm" />
            <span className="flex min-w-0 flex-col">
              <Link
                to="/bridges/offerings/$type"
                params={{ type: o.type }}
                className="truncate font-medium text-text hover:text-accent hover:underline"
              >
                {o.name ?? o.type}
              </Link>
              <span className="truncate text-xs text-text-muted">
                {o.mode === "shared" ? "One bridge for everyone" : "One per person"}
              </span>
            </span>
          </span>
        );
      },
      renderCompact: (o) => o.name ?? o.type,
    },
    {
      key: "runtime",
      header: "Runs",
      priority: 2,
      render: (o) => <span className="text-text">{runtimeMeta[o.runtime].label}</span>,
    },
    {
      key: "enabled",
      header: "Status",
      priority: 1,
      render: (o) =>
        o.enabled ? (
          <Badge status="success">Enabled</Badge>
        ) : (
          <Badge status="muted">Disabled</Badge>
        ),
      renderCompact: (o) => (o.enabled ? "Enabled" : "Disabled"),
    },
    {
      key: "front_door",
      header: "People message",
      priority: 2,
      interactive: true,
      render: (o) =>
        o.front_door ? (
          <CopyableId value={o.front_door} />
        ) : (
          <span className="text-text-muted">Nobody: it is shared</span>
        ),
    },
    {
      key: "instances",
      header: "Bridges",
      priority: 1,
      render: (o) => <InstanceCounts offering={o} />,
      renderCompact: (o) => `${totalInstances(o.instances)} bridges`,
    },
  ];

  if (!canRead) {
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Bridges</h1>
        <ForbiddenState scope="bridges:read" />
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h1 className="text-xl text-text">Bridges</h1>
          <p className="mt-0.5 text-sm text-text-muted">
            Other networks, for the people on this server. Each person who wants one gets their own
            bridge by messaging its bot.
          </p>
        </div>
        <Button
          disabled={!canWrite}
          title={!canWrite ? "Needs bridges:write" : undefined}
          onClick={() => navigate({ to: "/bridges/new" })}
        >
          Offer a bridge
        </Button>
      </div>

      <BridgesTabs current="offerings" />

      {target && !target.available && (
        <p className="mt-4 rounded-md border border-border bg-surface px-4 py-3 text-sm text-text-muted">
          <span className="font-medium text-text">This server can&apos;t run bridges itself.</span>{" "}
          {target.reason ?? "It isn't running in Kubernetes with the chart's bridges enabled."}{" "}
          Bridges offered here run elsewhere, from their files.
        </p>
      )}

      {isError ? (
        <div className="mt-6">
          <QueryProblemState
            error={error}
            resource="bridge offerings"
            scope="bridges:read"
            onRetry={() => refetch()}
          />
        </div>
      ) : (
        <div className="mt-4">
          <DataTable
            caption="Offered bridges"
            columns={columns}
            rows={rows}
            getRowId={(o) => o.type}
            loading={isLoading}
            onRowClick={(o) =>
              navigate({ to: "/bridges/offerings/$type", params: { type: o.type } })
            }
            empty={
              <EmptyState
                icon={<Cable aria-hidden="true" />}
                title="No bridges offered yet"
                description="Offer WhatsApp, Signal, Telegram, Discord, iMessage and more. Each person then gets their own bridge by messaging its bot, and signs in from there."
                docsHref="https://docs.mau.fi/bridges/"
                action={
                  canWrite ? (
                    <Button onClick={() => navigate({ to: "/bridges/new" })}>Offer a bridge</Button>
                  ) : undefined
                }
              />
            }
          />
        </div>
      )}
    </div>
  );
}

/** "3 Ready, 1 Starting, 1 Failed" as badges, or "None yet". */
function InstanceCounts({ offering }: { offering: BridgeOffering }) {
  const counts = instanceCountList(offering.instances);
  if (counts.length === 0) return <span className="text-text-muted">None yet</span>;
  return (
    <span className="flex flex-wrap gap-1">
      {counts.map(({ state, count }) => (
        <Badge key={state} status={instanceStateBadge(state)}>
          <span className="tabular-nums">{count}</span> {instanceStateLabel(state).toLowerCase()}
        </Badge>
      ))}
    </span>
  );
}
