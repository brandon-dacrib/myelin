import { InstanceSignIn } from "./InstanceSignIn";
import { useMemo, useState, type ReactNode } from "react";
import { useQueries } from "@tanstack/react-query";
import { Link, useParams } from "@tanstack/react-router";
import { ChevronLeft, Plus, Users } from "lucide-react";
import {
  appserviceLoginsQueryOptions,
  instanceUserSegment,
  useBridgeConnections,
  useDeleteBridgeConnection,
  useBridgeInstanceFiles,
  useBridgeInstances,
  useBridgeOffering,
  useBridgeTypes,
  useDeleteBridgeInstance,
  usePutBridgeInstance,
  type BridgeConnection,
  type BridgeInstance,
  type BridgeOffering,
  type BridgeType,
} from "@/api/bridges";
import { useServerInfo } from "@/api/dashboard";
import { ApiProblemError } from "@/api/problem";
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
import { getSession, hasScope } from "@/lib/auth";
import { nextStepsPhase, phaseLabel } from "@/lib/bridge-next-steps";
import {
  frontDoorSentence,
  imageTag,
  instanceAddress,
  instanceOwnerLabel,
  instanceStateBadge,
  instanceStateLabel,
  runsOnMac,
  runtimeMeta,
  deploymentPhaseMeta,
  takesPublicUrl,
} from "@/lib/bridge-offerings";
import {
  currentVolumeSize,
  describeVolumeUsage,
  volumeMessage,
  volumeNeedsAttention,
} from "@/lib/bridge-volume";
import { VolumeSizeDialog, VolumeStateBadge } from "../BridgeVolume";
import { AddInstanceDialog } from "./AddInstanceDialog";
import { AddWebhookDialog } from "./AddWebhookDialog";
import { InstanceFilesDialog } from "./InstanceFilesDialog";
import { InstanceNextSteps } from "./InstanceNextSteps";
import { MacSetupDialog } from "./MacSetupDialog";
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
  // The owner segment, not the instance: the dialog follows the live row as the page polls.
  const [macSetupFor, setMacSetupFor] = useState<string | null>(null);
  // The same for the volume dialog: it shows the live row's measurement.
  const [volumeFor, setVolumeFor] = useState<string | null>(null);
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
  const mac = runsOnMac(catalogueType);
  const volumeInstance =
    volumeFor === null
      ? null
      : (instances.find((i) => instanceUserSegment(i) === volumeFor) ?? null);
  const macSetupInstance =
    macSetupFor === null
      ? null
      : (instances.find((i) => instanceUserSegment(i) === macSetupFor) ?? null);

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

  function openMacSetup(instance: BridgeInstance) {
    setMacSetupFor(instanceUserSegment(instance));
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
        {mac ? (
          <Button
            variant="secondary"
            size="sm"
            aria-label={`Set up on a Mac for ${owner}`}
            disabled={!canWrite}
            title={noWrite ?? "Download the bridge, set the Mac's address and get its files"}
            onClick={() => openMacSetup(instance)}
          >
            Set up on a Mac
          </Button>
        ) : (
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
        )}
        {instance.volume && (
          <Button
            variant="ghost"
            size="sm"
            aria-label={`Volume for ${owner}`}
            title="How full its volume is, and a larger size"
            onClick={() => setVolumeFor(instanceUserSegment(instance))}
          >
            Volume
          </Button>
        )}
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
      key: "sign-in",
      header: `Signed in to ${name}`,
      priority: 2,
      interactive: true,
      render: (i) => <InstanceSignIn instance={i} />,
    },
    {
      key: "deployment",
      header: offering.runtime === "cluster" ? "Deployment" : "Runs",
      priority: 3,
      render: (i) => <DeploymentCell instance={i} runtime={offering.runtime} mac={mac} />,
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

      <FrontDoor offering={offering} mac={mac} />

      <OverlappingRegistrations offering={offering} />

      <SettingsSummary offering={offering} type={catalogueType} />

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
                mac={mac}
                actions={actions(instances[0])}
              />
            ) : (
              <p className="text-sm text-text-muted">
                Its bridge isn&apos;t set up yet. Saving its settings again sets it up.
              </p>
            )}
          </div>
          {takesPublicUrl(catalogueType) && (
            <ConnectionsSection
              type={offering.type}
              name={offering.name ?? offering.type}
              canWrite={canWrite}
            />
          )}
        </section>
      ) : (
        <section aria-labelledby="instances" className="mt-8">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <div>
              <h2 id="instances" className="text-md font-medium text-text">
                People&apos;s bridges
              </h2>
              <p className="mt-0.5 text-sm text-text-muted">
                {mac
                  ? "Each has its own registration and bot, and runs on that person's own Mac. "
                  : `Each has its own registration, bot and ${offering.runtime === "cluster" ? "pod" : "process"}. `}
                Removing one deletes that person&apos;s sign-ins and nothing else.
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
          {!instancesQuery.isError && (
            <NextStepsSection instances={instances} offering={offering} type={catalogueType} />
          )}
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

      <MacSetupDialog
        offering={offering}
        type={catalogueType}
        instance={macSetupInstance}
        files={files}
        onOpenChange={(open) => {
          if (!open) {
            setMacSetupFor(null);
            files.reset();
          }
        }}
        onRenderFiles={() => macSetupFor !== null && files.mutate({ type, userId: macSetupFor })}
      />

      <VolumeSizeDialog
        open={volumeInstance !== null}
        onOpenChange={(open) => !open && setVolumeFor(null)}
        volume={volumeInstance?.volume ?? null}
        type={type}
        userId={volumeFor ?? "_"}
        appserviceId={volumeInstance?.appservice_id}
        bridgeName={
          volumeInstance?.user_id ? `${instanceOwnerLabel(volumeInstance)}'s ${name} bridge` : name
        }
        canWrite={canWrite}
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
        type={catalogueType}
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
function FrontDoor({ offering, mac }: { offering: BridgeOffering; mac: boolean }) {
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
              : mac
                ? `It tells them an administrator runs ${name} for them. Their bridge appears below, waiting for the bridge on their Mac: Set up on a Mac on its row has the download, the Mac's address and the files to run there.`
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

/**
 * Bridges of this network registered by hand (the wizard's, or a registration file) whose
 * namespace covers this offering's instances: a message for a ghost reaches both, so each is
 * named with what to do (RFC 0017 section 6). Nothing once they are removed.
 */
function OverlappingRegistrations({ offering }: { offering: BridgeOffering }) {
  const overlaps = offering.overlapping_appservices ?? [];
  if (overlaps.length === 0) return null;
  return (
    <section
      aria-labelledby="overlapping"
      className="mt-6 rounded-md border border-warning-border bg-warning-bg px-4 py-3 text-sm text-warning"
      data-testid="overlapping-registrations"
    >
      <h2 id="overlapping" className="font-medium">
        Also registered by hand
      </h2>
      <ul className="mt-1 flex flex-col gap-1">
        {overlaps.map((o) => (
          <li key={o.id}>
            <Link
              to="/bridges/$bridgeId"
              params={{ bridgeId: o.id }}
              className="font-identifier underline underline-offset-2"
            >
              {o.id}
            </Link>
            : {o.detail}
          </li>
        ))}
      </ul>
    </section>
  );
}

function SettingsSummary({
  offering,
  type,
}: {
  offering: BridgeOffering;
  type: BridgeType | undefined;
}) {
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
        <Detail label="Runs">
          {runsOnMac(type) ? "On each person's Mac" : runtimeMeta[offering.runtime].label}
        </Detail>
        {runsOnMac(type) ? (
          <Detail label="Build">
            {type?.download_note ?? (
              <span className="font-identifier [overflow-wrap:anywhere]">
                {imageTag(offering.image)}
              </span>
            )}
          </Detail>
        ) : (
          <Detail label="Image">
            <span className="font-identifier [overflow-wrap:anywhere]">
              {offering.image ?? `tag ${imageTag(offering.image)}`}
            </span>
          </Detail>
        )}
        <Detail label="Who can have one">
          {offering.access?.all_local_users === false
            ? users.length > 0
              ? users.join(", ")
              : "Nobody yet"
            : "Everyone on this server"}
        </Detail>
        {takesPublicUrl(type) ? (
          <>
            <Detail label="Public URL">
              <span className="font-identifier [overflow-wrap:anywhere]">
                {offering.options?.public_url || "Not set"}
              </span>
              <span className="mt-0.5 block text-xs text-text-muted">
                Every webhook URL the bridge hands out starts with it; an Ingress for it points at
                the bridge&apos;s Service port &quot;webhooks&quot;.
              </span>
            </Detail>
            <Detail label="GitHub App">
              {offering.options?.github?.app_id
                ? `App ${offering.options.github.app_id}`
                : "None: repositories post through webhooks"}
            </Detail>
          </>
        ) : (
          <Detail label="Options">{options.length > 0 ? options.join(", ") : "None"}</Detail>
        )}
        {type?.command_prefix && (
          <Detail label="Command prefix">
            <code className="font-identifier">{type.command_prefix}</code>
            <span className="mt-0.5 block text-xs text-text-muted">
              Bare commands (<code className="font-identifier">login</code>) work in the chat a
              person invited the bot to; anywhere else they start with this.
            </span>
          </Detail>
        )}
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

/**
 * Beneath the table: who still has something to do, and the words to tell them (RFC 0017
 * section 4.1: the bot invites them to a chat with the steps when the bridge is ready; this is the
 * same for the operator to relay). One disclosure per person whose bridge is not signed in yet,
 * so a server where everyone has signed in says so in one line. Asks the bridges through the same
 * queries as the "Signed in to" column, so nothing is fetched twice.
 */
function NextStepsSection({
  instances,
  offering,
  type,
}: {
  instances: BridgeInstance[];
  offering: BridgeOffering;
  type: BridgeType | undefined;
}) {
  const owned = instances.filter((i) => i.user_id && i.state !== "removing");
  const logins = useQueries({
    queries: owned.map((i) =>
      appserviceLoginsQueryOptions(
        i.state === "ready" ? (i.appservice_id ?? undefined) : undefined,
        undefined,
      ),
    ),
  });
  const self = getSession()?.operator.subject;
  const pending = owned
    .map((instance, idx) => ({
      instance,
      phase: nextStepsPhase(instance, offering.runtime, logins[idx], type),
    }))
    .filter(({ phase }) => phase.kind !== "signed-in");
  if (owned.length === 0) return null;
  const name = offering.name ?? offering.type;
  return (
    <section aria-labelledby="next-steps" className="mt-6">
      <h3 id="next-steps" className="text-sm font-medium text-text">
        Next steps
      </h3>
      <p className="mt-0.5 text-sm text-text-muted">
        Who still has something to do, and the words to tell them. When a person&apos;s bridge is
        ready, its bot invites them to a chat and posts the sign-in steps there; these are the same
        steps, for you to relay when that message was missed.
      </p>
      {pending.length === 0 ? (
        <p className="mt-3 text-sm text-text">
          Everyone here has signed in to {name}. Nothing to tell anyone.
        </p>
      ) : (
        <ul className="mt-3 flex flex-col gap-2">
          {pending.map(({ instance, phase }) => {
            const owner = instanceOwnerLabel(instance);
            const isSelf = instance.user_id === self;
            return (
              <li key={instanceUserSegment(instance)}>
                <details className="rounded-md border border-border bg-surface">
                  <summary className="flex cursor-pointer flex-wrap items-center gap-2 px-4 py-3 text-sm text-text">
                    <span>
                      Next steps for <span className="font-identifier">{owner}</span>
                    </span>
                    <Badge
                      status={
                        phase.kind === "sign-in" ? "success" : instanceStateBadge(instance.state)
                      }
                    >
                      {phaseLabel(phase, isSelf)}
                    </Badge>
                    {isSelf && <span className="text-text-muted">(this is you)</span>}
                  </summary>
                  <div className="border-t border-border px-4 py-4">
                    <InstanceNextSteps instance={instance} offering={offering} type={type} />
                  </div>
                </details>
              </li>
            );
          })}
        </ul>
      )}
    </section>
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
    <span className="whitespace-nowrap text-text-muted">
      {!health || health === "unknown" ? "Not yet" : health}
    </span>
  );
}

function DeploymentCell({
  instance,
  runtime,
  mac,
}: {
  instance: BridgeInstance;
  runtime: BridgeOffering["runtime"];
  mac: boolean;
}) {
  const d = instance.deployment;
  if (!d && mac) {
    const address = instanceAddress(instance);
    return (
      <div className="flex flex-col gap-0.5">
        {address?.set ? (
          <span className="font-identifier text-text [overflow-wrap:anywhere]">{address.host}</span>
        ) : (
          <span className="text-text-muted">Address not set</span>
        )}
        <span className="text-xs text-text-muted">
          On {instance.user_id ? `${instance.user_id}'s` : "a"} Mac
        </span>
      </div>
    );
  }
  if (!d) {
    return (
      <span className="text-text-muted">{runtime === "elsewhere" ? "Elsewhere" : "Not yet"}</span>
    );
  }
  return (
    <div className="flex max-w-[15rem] flex-col gap-0.5">
      <span className="flex items-center gap-2 text-text">
        <Badge status={deploymentPhaseMeta(d.phase).status}>
          {deploymentPhaseMeta(d.phase).label}
        </Badge>
        <span className="text-text-muted">{d.name}</span>
      </span>
      {d.message && (
        <span className="font-identifier text-xs break-words text-text-muted">{d.message}</span>
      )}
      {instance.volume && <VolumeLine volume={instance.volume} />}
    </div>
  );
}

/**
 * The deployment's volume in a line: a nearly full or full one as a badge with the server's
 * message (a full volume stops the bridge), any other as how much is used. Volume in the row's
 * actions has the rest and the size control.
 */
function VolumeLine({ volume }: { volume: NonNullable<BridgeInstance["volume"]> }) {
  if (volumeNeedsAttention(volume)) {
    return (
      <span className="flex flex-col items-start gap-0.5" data-testid="volume-line">
        <VolumeStateBadge volume={volume} />
        <span className="text-xs text-text-muted">{volumeMessage(volume)}</span>
      </span>
    );
  }
  const usage = describeVolumeUsage(volume);
  return (
    <span className="text-xs text-text-muted" data-testid="volume-line">
      Volume {usage ?? `${currentVolumeSize(volume)}, not measured yet`}
    </span>
  );
}

/** A shared offering's one bridge, as a status panel rather than a table of one. */
/** The services a connection comes from, in words, with a badge each. */
const CONNECTION_SERVICES: Record<string, string> = {
  webhook: "Webhook",
  github: "GitHub",
  gitlab: "GitLab",
  jira: "Jira",
  feed: "Feed",
  figma: "Figma",
  openproject: "OpenProject",
};

/**
 * What a shared integrations bridge (Hookshot) has connected, room by room: one bridge, many
 * connections, each a state event in the room it posts into, read by the server from the rooms
 * its bot is in (Hookshot 7 has no provisioning API). A webhook is added here (the server makes
 * it as the bot, from a source it has a transformation for, and shows the URL once) or in the
 * room (`!hookshot webhook <name>`, `!hookshot github repo org/repo`, `!hookshot feed <url>`);
 * any active connection can be removed here. A webhook's secret URL is not in the state and so
 * not listed.
 */
function ConnectionsSection({
  type,
  name,
  canWrite,
}: {
  type: string;
  name: string;
  canWrite: boolean;
}) {
  const query = useBridgeConnections(type, true);
  const rows = query.data?.items ?? [];
  const rooms = query.data?.rooms ?? [];
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<BridgeConnection | null>(null);
  const columns: Column<BridgeConnection>[] = [
    {
      key: "service",
      header: "Service",
      priority: 1,
      render: (c) => (
        <Badge status={c.state === "removed" ? "muted" : "neutral"}>
          {CONNECTION_SERVICES[c.service] ?? c.service}
        </Badge>
      ),
    },
    {
      key: "name",
      header: "Connection",
      priority: 1,
      render: (c) => (
        <span>
          <span className="font-identifier [overflow-wrap:anywhere]">{c.name}</span>
          {c.source && (
            <span className="mt-0.5 block text-xs text-text-muted">
              Source: <span className="font-identifier">{c.source}</span>
            </span>
          )}
        </span>
      ),
    },
    {
      key: "room",
      header: "Room",
      priority: 1,
      render: (c) =>
        c.room_name ? (
          <span>
            {c.room_name}
            <span className="mt-0.5 block text-xs text-text-muted">
              <CopyableId value={c.room_id} />
            </span>
          </span>
        ) : (
          <CopyableId value={c.room_id} />
        ),
      interactive: true,
    },
    {
      key: "state",
      header: "State",
      priority: 2,
      render: (c) => (
        <Badge status={c.state === "active" ? "success" : "muted"}>
          {c.state === "active" ? "Active" : "Removed"}
        </Badge>
      ),
    },
    {
      key: "actions",
      header: "Actions",
      priority: 1,
      align: "end",
      interactive: true,
      render: (c) =>
        c.state === "active" ? (
          <Button
            variant="ghost"
            size="sm"
            disabled={!canWrite}
            title={!canWrite ? "Needs bridges:write" : undefined}
            aria-label={`Remove ${c.name}`}
            onClick={() => setRemoving(c)}
          >
            Remove
          </Button>
        ) : null,
    },
  ];
  return (
    <section aria-labelledby="connections" className="mt-6" data-testid="connections">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h3 id="connections" className="text-sm font-medium text-text">
          Connections
        </h3>
        <Button
          variant="secondary"
          size="sm"
          leadingIcon={<Plus size={14} aria-hidden="true" />}
          disabled={!canWrite}
          title={!canWrite ? "Needs bridges:write" : undefined}
          onClick={() => setAdding(true)}
        >
          Add a webhook
        </Button>
      </div>
      <p className="mt-1 text-sm text-text-muted">
        One {name} serves every room: each repository, feed or webhook is a connection in the room
        it posts into. A webhook is added here, shaped for what will post to it (Datadog, GitHub,
        Linear, Notion, Gmail, Alertmanager, or plain JSON), or in the room with the bot&apos;s
        commands; its URL is shown once, when it is made.
      </p>
      {adding && (
        <AddWebhookDialog
          type={type}
          bridgeName={name}
          rooms={rooms}
          open={adding}
          onOpenChange={setAdding}
        />
      )}
      {removing && (
        <RemoveConnectionDialog
          type={type}
          connection={removing}
          open={Boolean(removing)}
          onOpenChange={(open) => {
            if (!open) setRemoving(null);
          }}
        />
      )}
      <div className="mt-3">
        {query.isError ? (
          <QueryProblemState
            error={query.error}
            resource="connections"
            scope="bridges:read"
            onRetry={() => query.refetch()}
          />
        ) : query.isLoading ? (
          <SkeletonText lines={2} />
        ) : rows.length === 0 ? (
          <p className="text-sm text-text-muted" data-testid="connections-empty">
            Nothing is connected yet. Invite the bot to a room (it needs to be allowed to send state
            events) and send <code className="font-identifier">!hookshot help</code>.
          </p>
        ) : (
          <DataTable
            columns={columns}
            rows={rows}
            getRowId={(c) => `${c.room_id}/${c.event_type}/${c.state_key}`}
            caption={`${name} connections`}
            density="compact"
          />
        )}
      </div>
    </section>
  );
}

/** Removes a connection the way the bridge does, after saying what that means. */
function RemoveConnectionDialog({
  type,
  connection,
  open,
  onOpenChange,
}: {
  type: string;
  connection: BridgeConnection;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const remove = useDeleteBridgeConnection();
  const [error, setError] = useState<string | null>(null);
  const webhook = connection.service === "webhook";
  async function confirm() {
    setError(null);
    try {
      await remove.mutateAsync({
        type,
        roomId: connection.room_id,
        stateKey: connection.state_key,
      });
      toast({ title: `${connection.name} is disconnected` });
      onOpenChange(false);
    } catch (err) {
      setError(
        err instanceof ApiProblemError
          ? (err.problem.detail ?? err.problem.title ?? "The server refused.")
          : "Couldn’t reach the server.",
      );
    }
  }
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        title={`Remove ${connection.name}?`}
        description={
          webhook
            ? `Its URL stops working: whatever posts to it gets an error, and nothing more lands in ${connection.room_name ?? connection.room_id}. The sender's own setup is not touched.`
            : `Nothing more from it lands in ${connection.room_name ?? connection.room_id}. The connection's state event is emptied, as the bridge's own remove command does.`
        }
        footer={
          <>
            <Button variant="secondary" onClick={() => onOpenChange(false)}>
              Keep it
            </Button>
            <Button variant="danger" disabled={remove.isPending} onClick={confirm}>
              {remove.isPending ? "Removing..." : "Remove"}
            </Button>
          </>
        }
      >
        {error && (
          <p role="alert" className="text-sm text-danger">
            {error}
          </p>
        )}
      </DialogContent>
    </Dialog>
  );
}

function SharedInstancePanel({
  instance,
  runtime,
  mac,
  actions,
}: {
  instance: BridgeInstance;
  runtime: BridgeOffering["runtime"];
  mac: boolean;
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
          <DeploymentCell instance={instance} runtime={runtime} mac={mac} />
        </Detail>
        <Detail label="Bot">
          {instance.bot ? <CopyableId value={instance.bot} /> : <span>—</span>}
        </Detail>
        <Detail label="Ready since">
          {instance.ready_at ? <RelativeTime at={instance.ready_at} /> : "Not yet"}
        </Detail>
      </dl>
      {instance.appservice_id && (
        <p className="mt-3 text-sm text-text-muted">
          Many people use this bridge, so who has signed in is asked one person at a time:{" "}
          <Link
            to="/bridges/$bridgeId"
            params={{ bridgeId: instance.appservice_id }}
            hash="sign-in"
            className="text-accent underline hover:no-underline"
          >
            ask the bridge about someone
          </Link>
          .
        </p>
      )}
    </div>
  );
}
