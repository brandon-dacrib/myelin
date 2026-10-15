import { useState, type ReactNode } from "react";
import { useParams, useNavigate, useRouterState, Link } from "@tanstack/react-router";
import { Root, List, Trigger, Content } from "radix-ui/tabs";
import { ExternalLink, Eye, EyeOff, ChevronLeft } from "lucide-react";
import {
  DOUBLE_PUPPET_ID_SUFFIX,
  deriveDisplayName,
  doublePuppetBridgeId,
  isBuiltInAppservice,
  isSendsNothingAppservice,
  useAppservice,
  useAppserviceHealth,
  useAppserviceBacklog,
  useAppserviceRegistration,
  useBridgeInstances,
  useBridgeTypes,
  usePauseAppservice,
  useResumeAppservice,
  useRotateAppserviceTokens,
  useDeleteAppservice,
  usePingAppservice,
  instanceUserSegment,
  type AppService,
  type AppserviceNamespaces,
  type BridgeInstance,
  type BridgeVolume,
} from "@/api/bridges";
import { useServerInfo } from "@/api/dashboard";
import { ApiProblemError } from "@/api/problem";
import { BridgeGlyph } from "@/components/BridgeGlyph";
import { BridgeSignInGuide } from "@/components/BridgeSignInGuide";
import { BridgeSignInState } from "@/components/BridgeSignInState";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { Dialog, DialogTrigger, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { QueryProblemState } from "@/components/QueryProblemState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { CopyableId } from "@/components/CopyableId";
import { RelativeTime } from "@/components/RelativeTime";
import { toast } from "@/components/ui/toast/toast-store";
import { getSession, hasScope } from "@/lib/auth";
import { bridgeKind, bridgeTitle, bridgeTypeOf, botMatrixId } from "@/lib/bridge-catalogue";
import { bridgeHealthMeta, healthKeyOf } from "@/lib/bridge-state";
import { volumeMessage, volumeNeedsAttention, volumeStateMeta } from "@/lib/bridge-volume";
import { cn } from "@/lib/cn";
import { BridgeTransactions } from "./BridgeTransactions";
import { EditBridgeDialog } from "./EditBridgeDialog";
import { VolumeDetails, VolumeSizeForm, VolumeStateBadge } from "./BridgeVolume";

/** The health-reason box's colours, by the health badge's status. */
const REASON_BOX: Record<string, string> = {
  danger: "border-danger-border bg-danger-bg text-danger",
  warning: "border-warning-border bg-warning-bg text-warning",
  info: "border-info-border bg-info-bg text-info",
  muted: "border-muted-status-border bg-muted-status-bg text-muted-status",
  success: "border-success-border bg-success-bg text-success",
  neutral: "border-border bg-surface-sunken text-text",
};

const TABS = ["overview", "sign-in", "registration", "transactions", "danger"] as const;
const TAB_LABELS: Record<(typeof TABS)[number], string> = {
  overview: "Overview",
  "sign-in": "Sign in",
  registration: "Registration",
  transactions: "Transactions",
  danger: "Danger",
};

/**
 * `/bridges/:id` — bridge detail (information-architecture.md, Bridges >
 * Bridge detail). Reconciled 2026-09-18 against the real `AppService`
 * resource: see api/bridges.ts's doc comment for what changed. The Sign in
 * tab says who has signed in, as the bridge's own provisioning API reports it
 * (`appservices.logins`; a type without one keeps that itself, and the tab
 * says so), and how a person signs in to *this* bridge, from its catalogue
 * entry (`bridge_type`).
 */
export function BridgeDetailPage() {
  const { bridgeId } = useParams({ from: "/bridges/$bridgeId" });
  const navigate = useNavigate();
  const [revealTokens, setRevealTokens] = useState(false);
  const [editOpen, setEditOpen] = useState(false);
  // `#sign-in` (from the offering page) opens that tab.
  const hash = useRouterState({ select: (s) => s.location.hash });
  const [activeTab, setActiveTab] = useState<(typeof TABS)[number]>(
    (TABS as readonly string[]).includes(hash) ? (hash as (typeof TABS)[number]) : "overview",
  );
  const { data: bridge, isLoading, isError, error, refetch } = useAppservice(bridgeId);
  // A double-puppeting registration has no delivery, so its health and backlog are not asked for.
  // Which it is comes from the record (`double_puppet_for`), so they wait for it.
  const deliveredTo = bridge !== undefined && doublePuppetBridgeId(bridge) === undefined;
  const { data: health } = useAppserviceHealth(deliveredTo ? bridgeId : undefined);
  // A bridge that is pushed to may have a double-puppeting registration beside it (decision 0046);
  // the registrations list folds that into the bridge's row, so this page says what it is. The
  // server names it in `double_puppet` (null when there is none; absent from an older server).
  const doublePuppet =
    bridge !== undefined && !isSendsNothingAppservice(bridge) && !isBuiltInAppservice(bridge)
      ? bridge.double_puppet
      : undefined;
  const { data: types } = useBridgeTypes();
  const { data: server } = useServerInfo();
  const {
    data: backlog,
    isError: backlogIsError,
    error: backlogError,
  } = useAppserviceBacklog(deliveredTo ? bridgeId : undefined);
  const canWrite = hasScope("bridges:write");
  const {
    data: registration,
    isError: registrationIsError,
    error: registrationError,
  } = useAppserviceRegistration(bridgeId, activeTab === "registration" && canWrite);
  // A bridge this server deploys has a volume; setting its size goes through the offering's
  // instance, found among the type's instances by its registration's id.
  const volumeType = bridge?.volume ? (bridge.bridge_type ?? undefined) : undefined;
  const typeInstances = useBridgeInstances(volumeType);

  const pause = usePauseAppservice();
  const resume = useResumeAppservice();
  const rotate = useRotateAppserviceTokens();
  const del = useDeleteAppservice();
  const ping = usePingAppservice();

  if (!hasScope("bridges:read")) {
    return (
      <div className="p-6">
        <ForbiddenState scope="bridges:read" />
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

  if (isError || !bridge) {
    return (
      <div className="p-6">
        <QueryProblemState error={error} resource="this bridge" onRetry={() => refetch()} />
      </div>
    );
  }

  const doublePuppetFor = doublePuppetBridgeId(bridge);
  if (doublePuppetFor !== undefined) {
    return <DoublePuppetRegistration registration={bridge} bridgeId={doublePuppetFor} />;
  }

  // AppService.id is optional on the schema (no `required` list); this page
  // only ever has one fetched by its route param, so that param is the
  // reliable fallback if the resource ever omitted its own id.
  const id = bridge.id ?? bridgeId;
  const type = bridgeTypeOf(bridge, types);
  const name = bridgeTitle(bridge, type);
  const botId = botMatrixId(bridge.sender_localpart, server?.name);
  const meta = bridgeHealthMeta[healthKeyOf(bridge)];
  // The whole queue's counts when the server gives them (OpenAPI 0.1.11); the backlog page
  // below holds only its first 50 entries.
  const pendingCount =
    bridge.queue?.pending ?? (backlog?.items ?? []).filter((e) => !e.dead_lettered).length;
  const deadLetteredCount =
    bridge.queue?.dead_lettered ?? (backlog?.items ?? []).filter((e) => e.dead_lettered).length;
  // Why the health is what it is, in the server's words (OpenAPI 0.1.18); null when healthy.
  // `undefined` is a server older than that, whose last error is shown instead.
  const healthReason = health ? health.reason : bridge.health_reason;

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <Link
        to="/bridges/registrations"
        className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
      >
        <ChevronLeft size={14} aria-hidden="true" />
        Registrations
      </Link>

      <div className="mt-2 flex flex-wrap items-start justify-between gap-4">
        <div className="flex items-start gap-3">
          <BridgeGlyph category={type?.category} size="lg" className="mt-0.5" />
          <div>
            <div className="flex items-center gap-3">
              <h1 className="text-xl text-text">{name}</h1>
              <Badge status={meta.status}>{meta.label}</Badge>
              {volumeNeedsAttention(bridge.volume) && <VolumeStateBadge volume={bridge.volume} />}
            </div>
            <p className="mt-1 flex flex-wrap items-center gap-x-2 text-sm text-text-muted">
              <span>{bridgeKind(bridge, type)}</span>
              <span aria-hidden="true">&middot;</span>
              <CopyableId value={id} />
              {type?.docs_url && (
                <>
                  <span aria-hidden="true">&middot;</span>
                  <a
                    href={type.docs_url}
                    target="_blank"
                    rel="noreferrer"
                    className="inline-flex items-center gap-1 text-accent hover:underline"
                  >
                    <ExternalLink size={12} aria-hidden="true" />
                    Documentation
                  </a>
                </>
              )}
            </p>
          </div>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button
            variant="secondary"
            disabled={!canWrite}
            title={!canWrite ? "Needs bridges:write" : "URL, rate limiting and namespaces"}
            onClick={() => setEditOpen(true)}
          >
            Edit
          </Button>
          {editOpen && (
            <EditBridgeDialog
              key={id}
              bridge={bridge}
              name={name}
              open={editOpen}
              onOpenChange={setEditOpen}
            />
          )}
          <Button
            variant="secondary"
            disabled={!canWrite || ping.isPending || !bridge.url}
            title={
              !canWrite
                ? "Needs bridges:write"
                : !bridge.url
                  ? "The registration has no URL, so there is nothing to reach"
                  : "Sends the bridge an empty transaction now and records whether it answered"
            }
            onClick={() =>
              ping.mutate(id, {
                onSuccess: (after) => {
                  const key = healthKeyOf(after);
                  if (key === "healthy") toast({ title: `${name} answered` });
                  else
                    toast({
                      title: `${name} did not answer`,
                      description: "The reason is on the page as soon as it is read back.",
                      variant: "danger",
                    });
                },
                onError: () => toast({ title: `Couldn't ping ${name}`, variant: "danger" }),
              })
            }
          >
            {ping.isPending ? "Testing…" : "Test connection"}
          </Button>
          {bridge.paused ? (
            <Button
              variant="secondary"
              disabled={!canWrite}
              title={
                !canWrite
                  ? "Needs bridges:write"
                  : "Delivers what queued while it was paused, in order, then carries on"
              }
              onClick={() =>
                resume.mutate(id, {
                  onSuccess: () => toast({ title: `Bridge ${name} resumed` }),
                  onError: () => toast({ title: `Couldn't resume ${name}`, variant: "danger" }),
                })
              }
            >
              Resume
            </Button>
          ) : (
            <Button
              variant="secondary"
              disabled={!canWrite}
              title={
                !canWrite
                  ? "Needs bridges:write"
                  : "Stops delivering to the bridge; events queue here and are sent on Resume"
              }
              onClick={() =>
                pause.mutate(id, {
                  onSuccess: () => toast({ title: `Bridge ${name} paused` }),
                  onError: () => toast({ title: `Couldn't pause ${name}`, variant: "danger" }),
                })
              }
            >
              Pause
            </Button>
          )}
          <Dialog>
            <DialogTrigger asChild>
              <Button variant="secondary" disabled={!canWrite}>
                Rotate tokens
              </Button>
            </DialogTrigger>
            <DialogContent
              title={`Rotate tokens for ${name}?`}
              description="The bridge's current as_token and hs_token stop working immediately. Update the bridge's config with the new tokens before it reconnects."
              footer={
                <>
                  <DialogClose asChild>
                    <Button variant="secondary">Cancel</Button>
                  </DialogClose>
                  <DialogClose asChild>
                    <Button
                      variant="danger"
                      onClick={() =>
                        rotate.mutate(id, {
                          onSuccess: () => {
                            setActiveTab("registration");
                            setRevealTokens(true);
                            toast({ title: `Tokens rotated for ${name}` });
                          },
                          onError: () =>
                            toast({
                              title: `Couldn't rotate tokens for ${name}`,
                              variant: "danger",
                            }),
                        })
                      }
                    >
                      Rotate tokens
                    </Button>
                  </DialogClose>
                </>
              }
            />
          </Dialog>
        </div>
      </div>

      {volumeNeedsAttention(bridge.volume) && (
        <div
          className={cn(
            "mt-4 rounded-md border px-4 py-3 text-sm",
            bridge.volume.state === "full"
              ? "border-danger-border bg-danger-bg text-danger"
              : "border-warning-border bg-warning-bg text-warning",
          )}
          data-testid="volume-alert"
        >
          <span className="font-medium">{volumeStateMeta[bridge.volume.state].label}.</span>{" "}
          {volumeMessage(bridge.volume)}{" "}
          {activeTab === "overview"
            ? "Give it a larger size under Volume below."
            : "Give it a larger size under Volume on the Overview tab."}
        </div>
      )}

      {healthReason ? (
        <div
          className={cn("mt-4 rounded-md border px-4 py-3 text-sm", REASON_BOX[meta.status])}
          data-testid="health-reason"
        >
          <span className="font-medium">{meta.label}: </span>
          {healthReason}
        </div>
      ) : (
        healthReason === undefined &&
        health?.last_error && (
          <div className="mt-4 rounded-md border border-danger-border bg-danger-bg px-4 py-3 text-sm text-danger">
            {health.last_error}
          </div>
        )
      )}

      {health?.overlaps_offering && (
        <div
          className="mt-4 rounded-md border border-warning-border bg-warning-bg px-4 py-3 text-sm text-warning"
          data-testid="overlaps-offering"
        >
          {health.overlaps_offering.detail}{" "}
          <Link
            to="/bridges/offerings/$type"
            params={{ type: health.overlaps_offering.type }}
            className="underline underline-offset-2"
          >
            The {health.overlaps_offering.name} offering
          </Link>
        </div>
      )}

      {health?.last_key_withheld && (
        <div
          className="mt-4 rounded-md border border-warning-border bg-warning-bg px-4 py-3 text-sm text-warning"
          data-testid="key-withheld"
        >
          <RelativeTime at={health.last_key_withheld.at} />,{" "}
          <span className="font-identifier">{health.last_key_withheld.sender}</span>&apos;s chat app
          refused to share a message&apos;s keys with this bridge&apos;s{" "}
          <span className="font-identifier">{health.last_key_withheld.to_user_id}</span>, device{" "}
          <span className="font-identifier">{health.last_key_withheld.to_device_id}</span> (
          <code className="font-identifier">{health.last_key_withheld.code}</code>
          {health.last_key_withheld.reason ? `, ${health.last_key_withheld.reason}` : ""}), so the
          bridge could not read that message and said so in the chat.{" "}
          {health.last_key_withheld.code === "m.unverified"
            ? "The chat app excludes devices their owner has not cross-signed. A bridge the server manages gets its bot's device cross-signed; for one run by hand, its bot needs a cross-signing identity (a mautrix bridge: encryption.self_sign), or the person relaxes that rule for this chat."
            : "That code is the chat app's own rule for the bridge's device; its encryption settings say which."}
        </div>
      )}

      <Root
        value={activeTab}
        onValueChange={(v) => setActiveTab(v as (typeof TABS)[number])}
        className="mt-6"
      >
        <List aria-label="Bridge sections" className="flex gap-1 border-b border-border">
          {TABS.map((tab) => (
            <Trigger
              key={tab}
              value={tab}
              className={cn(
                "border-b-2 border-transparent px-3 py-2 text-sm font-medium text-text-muted",
                "data-[state=active]:border-accent data-[state=active]:text-accent",
                "hover:text-text focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
              )}
            >
              {TAB_LABELS[tab]}
            </Trigger>
          ))}
        </List>

        <Content value="overview" className="py-6">
          <div className="grid grid-cols-1 gap-3 sm:grid-cols-3">
            <Tile label="Connection">
              <div className="flex flex-wrap items-center gap-2">
                <Badge status={meta.status}>{meta.label}</Badge>
                <span className="text-sm text-text-muted">
                  last ping <RelativeTime at={health?.last_ping_at} />
                </span>
              </div>
            </Tile>
            <Tile label="Backlog">
              {backlogIsError ? (
                <QueryProblemState error={backlogError} resource="the backlog" compact />
              ) : pendingCount + deadLetteredCount === 0 ? (
                <span className="text-sm text-text">Nothing waiting</span>
              ) : (
                <span className="text-sm text-text">
                  {pendingCount} pending
                  {deadLetteredCount > 0 && (
                    <>
                      <span className="text-danger">, {deadLetteredCount} dead-lettered</span>{" "}
                      <button
                        type="button"
                        className="text-accent hover:underline"
                        onClick={() => setActiveTab("transactions")}
                      >
                        Review
                      </button>
                    </>
                  )}
                </span>
              )}
            </Tile>
            <Tile label="Bot">
              <CopyableId value={botId} />
            </Tile>
          </div>
          <dl className="mt-6 grid grid-cols-1 gap-x-8 gap-y-4 sm:grid-cols-2 xl:grid-cols-3">
            <Fact
              label="Reached at"
              value={<span className="font-identifier">{bridge.url ?? "never pushed to"}</span>}
            />
            <Fact label="Rate limited" value={bridge.rate_limited ? "Yes" : "No, exempt"} />
            <Fact label="Added" value={<RelativeTime at={bridge.created_at} />} />
            {doublePuppet !== undefined && (
              <Fact
                label="Double puppeting"
                value={
                  doublePuppet === null ? (
                    "Off: it posts as its own ghosts only"
                  ) : (
                    <>
                      On: it posts as its owner with{" "}
                      <Link
                        to="/bridges/$bridgeId"
                        params={{ bridgeId: doublePuppet }}
                        className="font-identifier text-accent hover:underline"
                      >
                        {doublePuppet}
                      </Link>
                      , a registration the server sends nothing
                    </>
                  )
                }
              />
            )}
          </dl>
          {bridge.volume && (
            <VolumeSection
              volume={bridge.volume}
              bridgeType={bridge.bridge_type ?? null}
              appserviceId={id}
              bridgeName={name}
              instances={typeInstances.data}
              instancesLoading={typeInstances.isLoading}
              instancesFailed={typeInstances.isError}
              canWrite={canWrite}
            />
          )}
          <h2 className="mt-6 text-sm font-medium text-text">Namespaces</h2>
          <p className="mt-1 text-sm text-text-muted">
            The user IDs, room aliases and room IDs this bridge speaks for. An exclusive rule means
            nobody but the bridge may create them. Change them with Edit.
          </p>
          <NamespaceList namespaces={bridge.namespaces} />
        </Content>

        <Content value="sign-in" className="py-6">
          <BridgeSignInState
            appserviceId={id}
            defaultUserId={getSession()?.operator.subject}
            bridgeUrl={bridge.url}
            canWrite={canWrite}
          />
          <BridgeSignInGuide type={type} botId={botId} />
          {bridge.links?.login_url && (
            <a
              href={bridge.links.login_url}
              target="_blank"
              rel="noreferrer"
              className="mt-3 inline-flex items-center gap-1 text-sm text-accent hover:underline"
            >
              <ExternalLink size={14} aria-hidden="true" />
              Open bridge login
            </a>
          )}
        </Content>

        <Content value="registration" className="py-6">
          {!canWrite ? (
            <ForbiddenState scope="bridges:write" />
          ) : registrationIsError ? (
            <QueryProblemState error={registrationError} resource="the registration" />
          ) : !registration ? (
            <SkeletonText lines={3} />
          ) : (
            <div className="flex flex-col gap-4">
              <div className="flex items-center gap-2">
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => setRevealTokens((v) => !v)}
                  leadingIcon={
                    revealTokens ? (
                      <EyeOff size={14} aria-hidden="true" />
                    ) : (
                      <Eye size={14} aria-hidden="true" />
                    )
                  }
                >
                  {revealTokens ? "Hide tokens" : "Reveal tokens"}
                </Button>
              </div>
              <pre className="overflow-x-auto rounded-md border border-border bg-surface-sunken p-3 font-identifier text-xs text-text">
                {revealTokens
                  ? JSON.stringify(registration, null, 2)
                  : JSON.stringify(redactTokens(registration), null, 2)}
              </pre>
            </div>
          )}
        </Content>

        <Content value="transactions" className="py-6">
          <BridgeTransactions
            id={id}
            name={name}
            canWrite={canWrite}
            entries={backlog?.items}
            queue={bridge.queue}
            isError={backlogIsError}
            error={backlogError}
          />
        </Content>

        <Content value="danger" className="py-6">
          <div className="rounded-md border border-danger-border bg-danger-bg p-4">
            <h3 className="text-md font-medium text-text">Remove this bridge</h3>
            <p className="mt-1 text-sm text-text-muted">
              The registration is removed and tokens are revoked immediately. This cannot be undone
              from here.
            </p>
            <Dialog>
              <DialogTrigger asChild>
                <Button variant="danger" className="mt-3" disabled={!canWrite}>
                  Remove bridge
                </Button>
              </DialogTrigger>
              <DialogContent
                title={`Remove ${name}?`}
                description="The registration is removed and tokens are revoked. This cannot be undone from here."
                footer={
                  <>
                    <DialogClose asChild>
                      <Button variant="secondary">Cancel</Button>
                    </DialogClose>
                    <Button
                      variant="danger"
                      onClick={() =>
                        del.mutate(id, {
                          onSuccess: () => {
                            toast({ title: `Bridge ${name} removed` });
                            navigate({ to: "/bridges/registrations" });
                          },
                          onError: () =>
                            toast({ title: `Couldn't remove ${name}`, variant: "danger" }),
                        })
                      }
                    >
                      Remove bridge
                    </Button>
                  </>
                }
              />
            </Dialog>
          </div>
        </Content>
      </Root>
    </div>
  );
}

/**
 * A bridge's double-puppeting registration (decision 0046), which is not a bridge: it has no URL,
 * the server sends it nothing, and the bridge only borrows its token to post as its owner. So it
 * has no health, queue, connection test, sign-in or transactions, and this page says what it is
 * for, whom it lets the bridge act as, and which bridge uses it.
 */
function DoublePuppetRegistration({
  registration,
  bridgeId,
}: {
  registration: AppService;
  bridgeId: string;
}) {
  const { data: bridge, error: bridgeError } = useAppservice(bridgeId);
  const { data: types } = useBridgeTypes();
  const navigate = useNavigate();
  const del = useDeleteAppservice();
  const canWrite = hasScope("bridges:write");
  // Left behind: its bridge was removed and it was not (the list still shows it as a row of its
  // own for that reason), so there is no bridge page to link to.
  const bridgeGone = bridgeError instanceof ApiProblemError && bridgeError.problem.status === 404;
  const bridgeName = bridge ? bridgeTitle(bridge, bridgeTypeOf(bridge, types)) : bridgeId;
  const id = registration.id ?? `${bridgeId}${DOUBLE_PUPPET_ID_SUFFIX}`;
  const bridgeLink = bridgeGone ? (
    <span>
      <span className="font-identifier">{bridgeId}</span>, a bridge no longer registered here
    </span>
  ) : (
    <Link to="/bridges/$bridgeId" params={{ bridgeId }} className="text-accent hover:underline">
      {bridgeName}
    </Link>
  );

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      {bridgeGone ? (
        <Link
          to="/bridges/registrations"
          className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
        >
          <ChevronLeft size={14} aria-hidden="true" />
          Registrations
        </Link>
      ) : (
        <Link
          to="/bridges/$bridgeId"
          params={{ bridgeId }}
          className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
        >
          <ChevronLeft size={14} aria-hidden="true" />
          {bridgeName}
        </Link>
      )}
      <div className="mt-2 flex items-center gap-3">
        <h1 className="text-xl text-text">{deriveDisplayName(registration)}</h1>
        <span title="The server's own registration; it is not probed.">
          <Badge status="muted">Built in</Badge>
        </span>
      </div>
      <p className="mt-1 text-sm text-text-muted">
        <CopyableId value={id} />
      </p>
      <p className="mt-4 max-w-3xl text-sm text-text">
        This is not a bridge. It is the key {bridgeLink} uses to post as its owner, so that what
        they send from the other network&apos;s own app shows up in Matrix from their own account
        rather than from a ghost. It has no URL, so the server never sends it anything: there is no
        connection, queue or health to look at here.{" "}
        {bridgeGone ? (
          <>
            Its bridge has been removed, so nothing uses it any more. Remove it below unless you
            mean to register that bridge again.
          </>
        ) : (
          <>Everything about the bridge itself is on {bridgeLink}&apos;s page.</>
        )}
      </p>
      <dl className="mt-6 grid grid-cols-1 gap-x-8 gap-y-4 sm:grid-cols-2 xl:grid-cols-3">
        <Fact label="Double puppeting for" value={bridgeLink} />
        <Fact label="Added" value={<RelativeTime at={registration.created_at} />} />
      </dl>
      <h2 className="mt-6 text-sm font-medium text-text">Who the bridge may post as</h2>
      <p className="mt-1 text-sm text-text-muted">
        The users this key covers. The claim is shared, not exclusive, and the key has no URL: it
        lets the bridge act as them, and nothing of theirs is sent to it.
      </p>
      <NamespaceList namespaces={registration.namespaces} />
      {bridgeGone && (
        <div className="mt-6 rounded-md border border-danger-border bg-danger-bg p-4">
          <h3 className="text-md font-medium text-text">Remove this registration</h3>
          <p className="mt-1 text-sm text-text-muted">
            Its token stops working at once, so nothing can post as these users with it any more.
          </p>
          <Dialog>
            <DialogTrigger asChild>
              <Button
                variant="danger"
                className="mt-3"
                disabled={!canWrite}
                title={!canWrite ? "Needs bridges:write" : undefined}
              >
                Remove registration
              </Button>
            </DialogTrigger>
            <DialogContent
              title={`Remove ${id}?`}
              description="The registration is removed and its token revoked. This cannot be undone from here."
              footer={
                <>
                  <DialogClose asChild>
                    <Button variant="secondary">Cancel</Button>
                  </DialogClose>
                  <Button
                    variant="danger"
                    onClick={() =>
                      del.mutate(id, {
                        onSuccess: () => {
                          toast({ title: `Registration ${id} removed` });
                          navigate({ to: "/bridges/registrations" });
                        },
                        onError: () => toast({ title: `Couldn't remove ${id}`, variant: "danger" }),
                      })
                    }
                  >
                    Remove registration
                  </Button>
                </>
              }
            />
          </Dialog>
        </div>
      )}
    </div>
  );
}

/**
 * The Overview tab's Volume section, for a bridge this server deploys: how full its volume is and
 * the control to give it a larger size. The size is set on the offering's instance (`PUT
 * .../instances/{user_id}`), which is found among its type's instances by this registration's id.
 */
function VolumeSection({
  volume,
  bridgeType,
  appserviceId,
  bridgeName,
  instances,
  instancesLoading,
  instancesFailed,
  canWrite,
}: {
  volume: BridgeVolume;
  bridgeType: string | null;
  appserviceId: string;
  bridgeName: string;
  instances: BridgeInstance[] | undefined;
  instancesLoading: boolean;
  instancesFailed: boolean;
  canWrite: boolean;
}) {
  const instance = instances?.find((i) => i.appservice_id === appserviceId);
  return (
    <section aria-labelledby="volume" className="mt-6">
      <h2 id="volume" className="text-sm font-medium text-text">
        Volume
      </h2>
      <p className="mt-1 text-sm text-text-muted">
        This server runs this bridge in the cluster, with a volume of its own mounted at{" "}
        <code className="font-identifier">/data</code>. The operator measures it about once a minute
        while the bridge runs.
      </p>
      <div className="mt-3 flex flex-col gap-5">
        <VolumeDetails volume={volume} />
        {bridgeType && instance ? (
          <VolumeSizeForm
            volume={volume}
            type={bridgeType}
            userId={instanceUserSegment(instance)}
            appserviceId={appserviceId}
            bridgeName={bridgeName}
            canWrite={canWrite}
          />
        ) : instancesLoading ? (
          <SkeletonText lines={2} />
        ) : (
          <p className="text-sm text-text-muted">
            {instancesFailed
              ? "Its offering's bridges could not be read, so its size cannot be set from here. "
              : "It is not among its offering's bridges, so its size cannot be set from here. "}
            {bridgeType && (
              <Link
                to="/bridges/offerings/$type"
                params={{ type: bridgeType }}
                className="text-accent hover:underline"
              >
                The offering&apos;s page
              </Link>
            )}
          </p>
        )}
      </div>
    </section>
  );
}

function redactTokens(registration: Record<string, unknown>): Record<string, unknown> {
  const redacted = { ...registration };
  for (const key of ["as_token", "hs_token"]) {
    if (key in redacted) redacted[key] = "•".repeat(24);
  }
  return redacted;
}

const NAMESPACE_LABELS = { users: "Users", aliases: "Aliases", rooms: "Rooms" } as const;

/** A registration's namespace rules, by category, each pattern with whether it is exclusive. */
function NamespaceList({ namespaces }: { namespaces: AppService["namespaces"] }) {
  const source = (namespaces ?? {}) as AppserviceNamespaces;
  const categories = (Object.keys(NAMESPACE_LABELS) as (keyof typeof NAMESPACE_LABELS)[]).map(
    (key) => [key, Array.isArray(source[key]) ? source[key] : []] as const,
  );
  if (categories.every(([, rules]) => rules.length === 0)) {
    return (
      <p className="mt-2 text-sm text-text-muted">
        None: the bridge speaks only as its own bot user.
      </p>
    );
  }
  return (
    <dl className="mt-3 grid grid-cols-1 gap-x-8 gap-y-3 sm:grid-cols-3">
      {categories.map(([key, rules]) => (
        <div key={key}>
          <dt className="text-xs text-text-muted">{NAMESPACE_LABELS[key]}</dt>
          <dd className="mt-0.5 text-sm text-text">
            {rules.length === 0 ? (
              <span className="text-text-muted">None</span>
            ) : (
              <ul className="flex flex-col gap-1">
                {rules.map((rule, i) => (
                  <li key={`${rule.regex}-${i}`} className="flex flex-wrap items-center gap-2">
                    <code className="font-identifier text-xs">{rule.regex}</code>
                    <span className="text-xs text-text-muted">
                      {rule.exclusive ? "exclusive" : "shared"}
                    </span>
                  </li>
                ))}
              </ul>
            )}
          </dd>
        </div>
      ))}
    </dl>
  );
}

function Tile({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="rounded-md border border-border bg-surface px-4 py-3">
      <p className="text-xs text-text-muted">{label}</p>
      <div className="mt-1.5">{children}</div>
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
