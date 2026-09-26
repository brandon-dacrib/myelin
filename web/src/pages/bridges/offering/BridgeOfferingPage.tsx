import { useMemo, useState, type ReactNode } from "react";
import { Link, useParams } from "@tanstack/react-router";
import { ChevronLeft, Users } from "lucide-react";
import {
  instanceUserSegment,
  useBridgeInstanceFiles,
  useBridgeInstances,
  useBridgeOffering,
  useBridgeTypes,
  useDeleteBridgeInstance,
  usePutBridgeInstance,
  type BridgeInstance,
  type BridgeOffering,
} from "@/api/bridges";
import { useServerInfo } from "@/api/dashboard";
import { BridgeGlyph } from "@/components/BridgeGlyph";
import { CopyableId } from "@/components/CopyableId";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { Badge, type BadgeProps } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import {
  frontDoorSentence,
  imageTag,
  instanceOwnerLabel,
  instanceStateBadge,
  instanceStateLabel,
  runtimeMeta,
} from "@/lib/bridge-offerings";
import { AddInstanceDialog } from "./AddInstanceDialog";
import { InstanceFilesDialog } from "./InstanceFilesDialog";
import { OfferingSettingsDialog } from "./OfferingSettingsDialog";
import { RemoveOfferingDialog } from "./RemoveOfferingDialog";

/** What needs a look first: failed, then anything on its way, then ready, then leaving. */
const ATTENTION: Record<string, number> = {
  failed: 0,
  requested: 1,
  registered: 1,
  deploying: 1,
  starting: 1,
  ready: 2,
  removing: 3,
};

const HEALTH: Record<string, { status: NonNullable<BadgeProps["status"]>; label: string }> = {
  healthy: { status: "success", label: "Healthy" },
  degraded: { status: "warning", label: "Degraded" },
  down: { status: "danger", label: "Down" },
  paused: { status: "muted", label: "Paused" },
};

/**
 * `/bridges/offerings/$type` -- one offered bridge (RFC 0017): what to tell people, how it is
 * set up, and everybody's bridge with what it is doing. Polls every five seconds while any
 * bridge is on its way (`useBridgeInstances`).
 */
export function BridgeOfferingPage() {
  const { type } = useParams({ from: "/bridges/offerings/$type" });
  const canRead = hasScope("bridges:read");
  const canWrite = hasScope("bridges:write");
  const offeringQuery = useBridgeOffering(type);
  const instancesQuery = useBridgeInstances(type);
  const { data: types } = useBridgeTypes();
  const { data: server } = useServerInfo();
  const putInstance = usePutBridgeInstance();
  const deleteInstance = useDeleteBridgeInstance();
  const files = useBridgeInstanceFiles();

  const [filesFor, setFilesFor] = useState<BridgeInstance | null>(null);
  const [removing, setRemoving] = useState<BridgeInstance | null>(null);
  const [adding, setAdding] = useState(false);
  const [editing, setEditing] = useState(false);
  const [stopping, setStopping] = useState(false);

  const instances = useMemo(
    () =>
      [...(instancesQuery.data ?? [])].sort(
        (a, b) =>
          (ATTENTION[a.state] ?? 9) - (ATTENTION[b.state] ?? 9) ||
          instanceOwnerLabel(a).localeCompare(instanceOwnerLabel(b)),
      ),
    [instancesQuery.data],
  );

  if (!canRead) {
    return (
      <div className="p-6">
        <ForbiddenState scope="bridges:read" />
      </div>
    );
  }

  const offering = offeringQuery.data;
  const catalogueType = types?.find((t) => t.id === type);

  if (offeringQuery.isError) {
    return (
      <div className="mx-auto max-w-[90rem] p-6">
        <BackLink />
        <div className="mt-6">
          <QueryProblemState
            error={offeringQuery.error}
            resource="bridge offering"
            scope="bridges:read"
            onRetry={() => offeringQuery.refetch()}
          />
        </div>
      </div>
    );
  }

  if (!offering) {
    return (
      <div className="mx-auto max-w-[90rem] p-6">
        <BackLink />
        <div className="mt-6">
          <SkeletonText lines={6} />
        </div>
      </div>
    );
  }

  const name = offering.name ?? offering.type;

  function openFiles(instance: BridgeInstance) {
    setFilesFor(instance);
    files.reset();
    files.mutate({ type, userId: instanceUserSegment(instance) });
  }

  function retry(instance: BridgeInstance) {
    const owner = instanceOwnerLabel(instance);
    putInstance.mutate(
      { type, userId: instanceUserSegment(instance) },
      {
        onSuccess: () => toast({ title: `Retrying ${owner}'s ${name} bridge` }),
        onError: () => toast({ title: `Couldn't retry ${owner}'s bridge`, variant: "danger" }),
      },
    );
  }

  const actions = (instance: BridgeInstance) => {
    const owner = instanceOwnerLabel(instance);
    const noWrite = !canWrite ? "Needs bridges:write" : undefined;
    return (
      <div className="flex justify-end gap-1 whitespace-nowrap">
        {instance.state === "failed" && (
          <Button
            variant="secondary"
            size="sm"
            aria-label={`Retry ${owner}`}
            disabled={!canWrite || putInstance.isPending}
            title={noWrite}
            onClick={() => retry(instance)}
          >
            Retry
          </Button>
        )}
        <Button
          variant="ghost"
          size="sm"
          aria-label={`Files for ${owner}`}
          disabled={!canWrite}
          title={noWrite ?? "Download the files to run it elsewhere"}
          onClick={() => openFiles(instance)}
        >
          Files
        </Button>
        {instance.user_id && (
          <Button
            variant="ghost"
            size="sm"
            aria-label={`Remove ${owner}`}
            disabled={!canWrite || instance.state === "removing"}
            title={noWrite}
            onClick={() => setRemoving(instance)}
            className="text-danger"
          >
            Remove
          </Button>
        )}
      </div>
    );
  };

  const columns: Column<BridgeInstance>[] = [
    {
      key: "user",
      header: "Person",
      priority: 1,
      interactive: true,
      render: (i) =>
        i.user_id ? <CopyableId value={i.user_id} /> : <span>{instanceOwnerLabel(i)}</span>,
      renderCompact: (i) => instanceOwnerLabel(i),
    },
    {
      key: "state",
      header: "State",
      priority: 1,
      render: (i) => <StateCell instance={i} />,
      renderCompact: (i) => instanceStateLabel(i.state),
    },
    {
      key: "health",
      header: "Ping",
      priority: 2,
      render: (i) => <HealthCell health={i.health} />,
    },
    {
      key: "deployment",
      header: offering.runtime === "cluster" ? "Deployment" : "Runs",
      priority: 3,
      render: (i) => <DeploymentCell instance={i} runtime={offering.runtime} />,
    },
    {
      key: "times",
      header: "When",
      priority: 3,
      render: (i) => (
        <div className="flex flex-col gap-0.5 whitespace-nowrap text-xs">
          <span>
            <span className="text-text-muted">Asked </span>
            <RelativeTime at={i.created_at} />
          </span>
          {i.ready_at && (
            <span>
              <span className="text-text-muted">Ready </span>
              <RelativeTime at={i.ready_at} />
            </span>
          )}
        </div>
      ),
    },
    {
      key: "actions",
      header: "Actions",
      priority: 1,
      align: "end",
      interactive: true,
      render: actions,
    },
  ];

  const shared = offering.mode === "shared";

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <BackLink />

      <div className="mt-2 flex flex-wrap items-start justify-between gap-4">
        <div className="flex items-start gap-3">
          <BridgeGlyph category={catalogueType?.category} size="lg" className="mt-0.5" />
          <div>
            <h1 className="text-xl text-text">{name}</h1>
            <div className="mt-1 flex flex-wrap items-center gap-2 text-sm text-text-muted">
              {offering.enabled ? (
                <Badge status="success">Offered</Badge>
              ) : (
                <Badge status="muted">Disabled</Badge>
              )}
              <span>{runtimeMeta[offering.runtime].label}</span>
              <span aria-hidden="true">·</span>
              <span>{shared ? "One bridge for everyone" : "One per person"}</span>
            </div>
          </div>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button
            variant="secondary"
            disabled={!canWrite}
            title={!canWrite ? "Needs bridges:write" : undefined}
            onClick={() => setEditing(true)}
          >
            Edit settings
          </Button>
          <Button
            variant="secondary"
            disabled={!canWrite}
            title={!canWrite ? "Needs bridges:write" : undefined}
            onClick={() => setStopping(true)}
            className="text-danger"
          >
            Stop offering
          </Button>
        </div>
      </div>

      <FrontDoor offering={offering} />

      <SettingsSummary offering={offering} />

      {shared ? (
        <section aria-labelledby="shared-bridge" className="mt-8">
          <h2 id="shared-bridge" className="text-md font-medium text-text">
            The bridge
          </h2>
          <div className="mt-3">
            {instancesQuery.isError ? (
              <QueryProblemState
                error={instancesQuery.error}
                resource="bridge"
                scope="bridges:read"
                onRetry={() => instancesQuery.refetch()}
              />
            ) : instancesQuery.isLoading ? (
              <SkeletonText lines={3} />
            ) : instances[0] ? (
              <SharedInstancePanel
                instance={instances[0]}
                runtime={offering.runtime}
                actions={actions(instances[0])}
              />
            ) : (
              <p className="text-sm text-text-muted">
                Its bridge isn&apos;t set up yet. Saving its settings again sets it up.
              </p>
            )}
          </div>
        </section>
      ) : (
        <section aria-labelledby="instances" className="mt-8">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <h2 id="instances" className="text-md font-medium text-text">
                People&apos;s bridges
              </h2>
              <p className="mt-0.5 text-sm text-text-muted">
                Each has its own registration, bot and{" "}
                {offering.runtime === "cluster" ? "pod" : "process"}. Removing one deletes that
                person&apos;s sign-ins and nothing else.
              </p>
            </div>
            <Button
              disabled={!canWrite}
              title={!canWrite ? "Needs bridges:write" : undefined}
              onClick={() => setAdding(true)}
            >
              Add for a user
            </Button>
          </div>
          <div className="mt-4">
            {instancesQuery.isError ? (
              <QueryProblemState
                error={instancesQuery.error}
                resource="bridges"
                scope="bridges:read"
                onRetry={() => instancesQuery.refetch()}
              />
            ) : (
              <DataTable
                caption={`People's ${name} bridges`}
                columns={columns}
                rows={instances}
                getRowId={(i) => instanceUserSegment(i)}
                loading={instancesQuery.isLoading}
                empty={
                  <EmptyState
                    icon={<Users aria-hidden="true" />}
                    title="Nobody has one yet"
                    description={
                      offering.front_door
                        ? `People get theirs by messaging ${offering.front_door}. You can also add one for someone.`
                        : "You can add one for someone."
                    }
                  />
                }
              />
            )}
          </div>
        </section>
      )}

      <InstanceFilesDialog
        offering={offering}
        instance={filesFor}
        files={files}
        onOpenChange={(open) => {
          if (!open) {
            setFilesFor(null);
            files.reset();
          }
        }}
        onRetry={() => filesFor && files.mutate({ type, userId: instanceUserSegment(filesFor) })}
      />

      <Dialog open={removing !== null} onOpenChange={(open) => !open && setRemoving(null)}>
        <DialogContent
          title={`Remove ${removing ? instanceOwnerLabel(removing) : ""}'s ${name} bridge?`}
          description={`This deletes their bridge${offering.runtime === "cluster" ? ", its pod and its volume" : " and its registration"}, and with it their ${name} sign-in: they have to link ${name} again from scratch. Everyone else's bridge is untouched.`}
          footer={
            <>
              <DialogClose asChild>
                <Button variant="secondary">Cancel</Button>
              </DialogClose>
              <Button
                variant="danger"
                disabled={deleteInstance.isPending}
                onClick={() => {
                  if (!removing) return;
                  const owner = instanceOwnerLabel(removing);
                  deleteInstance.mutate(
                    { type, userId: instanceUserSegment(removing) },
                    {
                      onSuccess: () => {
                        toast({ title: `Removed ${owner}'s ${name} bridge` });
                        setRemoving(null);
                      },
                      onError: () =>
                        toast({ title: `Couldn't remove ${owner}'s bridge`, variant: "danger" }),
                    },
                  );
                }}
              >
                {deleteInstance.isPending ? "Removing..." : "Remove bridge"}
              </Button>
            </>
          }
        />
      </Dialog>

      <AddInstanceDialog
        offering={offering}
        open={adding}
        onOpenChange={setAdding}
        serverName={server?.name}
      />
      <OfferingSettingsDialog
        offering={offering}
        type={catalogueType}
        open={editing}
        onOpenChange={setEditing}
        serverName={server?.name}
      />
      <RemoveOfferingDialog offering={offering} open={stopping} onOpenChange={setStopping} />
    </div>
  );
}

function BackLink() {
  return (
    <Link
      to="/bridges"
      className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
    >
      <ChevronLeft size={14} aria-hidden="true" />
      Bridges
    </Link>
  );
}

/** The one line an administrator tells people, with the address to copy. */
function FrontDoor({ offering }: { offering: BridgeOffering }) {
  const sentence = frontDoorSentence(offering);
  const name = offering.name ?? offering.type;
  return (
    <section
      aria-labelledby="front-door"
      className="mt-6 rounded-md border border-border bg-surface px-4 py-4"
    >
      <h2 id="front-door" className="text-sm font-medium text-text">
        {offering.mode === "shared" ? "How people use it" : "How people get one"}
      </h2>
      {offering.front_door ? (
        <>
          <p className="mt-1 text-base text-text">{sentence}</p>
          <p className="mt-2 flex flex-wrap items-center gap-2 text-sm text-text-muted">
            Its bot: <CopyableId value={offering.front_door} />
          </p>
          <p className="mt-2 text-sm text-text-muted">
            {offering.runtime === "cluster"
              ? `It sets up their bridge, and invites them to it a minute or two later.`
              : `It tells them an administrator runs ${name} for them. Their bridge appears below, waiting for its first ping: download its files and run it where it can run.`}
          </p>
        </>
      ) : (
        <p className="mt-1 text-sm text-text-muted">
          One {name} bridge serves everyone allowed; there is nothing to ask for.
        </p>
      )}
      {!offering.enabled && (
        <p className="mt-2 text-sm text-warning">
          Disabled: its bot turns new people away. Bridges people already have keep running.
        </p>
      )}
    </section>
  );
}

function SettingsSummary({ offering }: { offering: BridgeOffering }) {
  const options = [
    offering.options?.encryption && "Encryption",
    offering.options?.double_puppeting && "Double puppeting",
    offering.options?.backfill && "Backfill",
  ].filter(Boolean);
  const users = offering.access?.users ?? [];
  return (
    <section aria-labelledby="settings" className="mt-8">
      <h2 id="settings" className="text-md font-medium text-text">
        Settings
      </h2>
      <dl className="mt-3 grid grid-cols-1 gap-x-8 gap-y-3 text-sm sm:grid-cols-2 xl:grid-cols-4">
        <Detail label="Runs">{runtimeMeta[offering.runtime].label}</Detail>
        <Detail label="Image">
          <span className="font-identifier break-all">
            {offering.image ?? `tag ${imageTag(offering.image)}`}
          </span>
        </Detail>
        <Detail label="Who can have one">
          {offering.access?.all_local_users === false
            ? users.length > 0
              ? users.join(", ")
              : "Nobody yet"
            : "Everyone on this server"}
        </Detail>
        <Detail label="Options">{options.length > 0 ? options.join(", ") : "None"}</Detail>
      </dl>
    </section>
  );
}

function Detail({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div>
      <dt className="text-xs text-text-muted">{label}</dt>
      <dd className="mt-0.5 text-text">{children}</dd>
    </div>
  );
}

function StateCell({ instance }: { instance: BridgeInstance }) {
  return (
    <div className="flex flex-col items-start gap-1">
      <Badge status={instanceStateBadge(instance.state)}>
        {instanceStateLabel(instance.state)}
      </Badge>
      {instance.reason && (
        <span className="max-w-xs text-xs text-text-muted">{instance.reason}</span>
      )}
    </div>
  );
}

function HealthCell({ health }: { health: string | null | undefined }) {
  const meta = health ? HEALTH[health] : undefined;
  if (meta) return <Badge status={meta.status}>{meta.label}</Badge>;
  return (
    <span className="text-text-muted">
      {!health || health === "unknown" ? "No answer yet" : health}
    </span>
  );
}

function DeploymentCell({
  instance,
  runtime,
}: {
  instance: BridgeInstance;
  runtime: BridgeOffering["runtime"];
}) {
  const d = instance.deployment;
  if (!d) {
    return (
      <span className="text-text-muted">{runtime === "elsewhere" ? "Elsewhere" : "Not yet"}</span>
    );
  }
  return (
    <div className="flex max-w-xs flex-col gap-0.5">
      <span className="text-text">
        {d.phase}
        <span className="text-text-muted"> · {d.name}</span>
      </span>
      {d.message && (
        <span className="font-identifier text-xs break-words text-text-muted">{d.message}</span>
      )}
    </div>
  );
}

/** A shared offering's one bridge, as a status panel rather than a table of one. */
function SharedInstancePanel({
  instance,
  runtime,
  actions,
}: {
  instance: BridgeInstance;
  runtime: BridgeOffering["runtime"];
  actions: ReactNode;
}) {
  return (
    <div className="rounded-md border border-border bg-surface p-4">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <StateCell instance={instance} />
        {actions}
      </div>
      <dl className="mt-4 grid grid-cols-1 gap-x-8 gap-y-3 text-sm sm:grid-cols-2 xl:grid-cols-4">
        <Detail label="Ping">
          <HealthCell health={instance.health} />
        </Detail>
        <Detail label={runtime === "cluster" ? "Deployment" : "Runs"}>
          <DeploymentCell instance={instance} runtime={runtime} />
        </Detail>
        <Detail label="Bot">
          {instance.bot ? <CopyableId value={instance.bot} /> : <span>—</span>}
        </Detail>
        <Detail label="Ready since">
          {instance.ready_at ? <RelativeTime at={instance.ready_at} /> : "Not yet"}
        </Detail>
      </dl>
    </div>
  );
}
