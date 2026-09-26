import { useState, type ReactNode } from "react";
import { useParams, useNavigate, Link } from "@tanstack/react-router";
import { Root, List, Trigger, Content } from "radix-ui/tabs";
import { ExternalLink, Eye, EyeOff, ChevronLeft } from "lucide-react";
import {
  useAppservice,
  useAppserviceHealth,
  useAppserviceBacklog,
  useAppserviceRegistration,
  useBridgeTypes,
  usePauseAppservice,
  useResumeAppservice,
  useRotateAppserviceTokens,
  useReplayAppserviceBacklog,
  useDeleteAppservice,
} from "@/api/bridges";
import { useServerInfo } from "@/api/dashboard";
import { BridgeGlyph } from "@/components/BridgeGlyph";
import { BridgeSignInGuide } from "@/components/BridgeSignInGuide";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { Dialog, DialogTrigger, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { QueryProblemState } from "@/components/QueryProblemState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { CopyableId } from "@/components/CopyableId";
import { RelativeTime } from "@/components/RelativeTime";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import { bridgeKind, bridgeTitle, bridgeTypeOf, botMatrixId } from "@/lib/bridge-catalogue";
import { bridgeHealthMeta, formatBacklogEntry, healthKeyOf } from "@/lib/bridge-state";
import { cn } from "@/lib/cn";

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
 * tab says how a person signs in to *this* bridge, from its catalogue entry
 * (`bridge_type`), rather than fabricating per-user login state the API does
 * not expose.
 */
export function BridgeDetailPage() {
  const { bridgeId } = useParams({ from: "/bridges/$bridgeId" });
  const navigate = useNavigate();
  const [revealTokens, setRevealTokens] = useState(false);
  const [activeTab, setActiveTab] = useState<(typeof TABS)[number]>("overview");
  const { data: bridge, isLoading, isError, error, refetch } = useAppservice(bridgeId);
  const { data: health } = useAppserviceHealth(bridgeId);
  const { data: types } = useBridgeTypes();
  const { data: server } = useServerInfo();
  const {
    data: backlog,
    isError: backlogIsError,
    error: backlogError,
  } = useAppserviceBacklog(bridgeId);
  const canWrite = hasScope("bridges:write");
  const {
    data: registration,
    isError: registrationIsError,
    error: registrationError,
  } = useAppserviceRegistration(bridgeId, activeTab === "registration" && canWrite);

  const pause = usePauseAppservice();
  const resume = useResumeAppservice();
  const rotate = useRotateAppserviceTokens();
  const replay = useReplayAppserviceBacklog();
  const del = useDeleteAppservice();

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

  // AppService.id is optional on the schema (no `required` list); this page
  // only ever has one fetched by its route param, so that param is the
  // reliable fallback if the resource ever omitted its own id.
  const id = bridge.id ?? bridgeId;
  const type = bridgeTypeOf(bridge, types);
  const name = bridgeTitle(bridge, type);
  const botId = botMatrixId(bridge.sender_localpart, server?.name);
  const meta = bridgeHealthMeta[healthKeyOf(bridge)];
  const pendingBacklog = (backlog?.items ?? []).filter((e) => !e.dead_lettered);
  const deadLettered = (backlog?.items ?? []).filter((e) => e.dead_lettered);

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <Link
        to="/bridges"
        className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
      >
        <ChevronLeft size={14} aria-hidden="true" />
        Bridges
      </Link>

      <div className="mt-2 flex flex-wrap items-start justify-between gap-4">
        <div className="flex items-start gap-3">
          <BridgeGlyph category={type?.category} size="lg" className="mt-0.5" />
          <div>
            <div className="flex items-center gap-3">
              <h1 className="text-xl text-text">{name}</h1>
              <Badge status={meta.status}>{meta.label}</Badge>
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
          {bridge.paused ? (
            <Button
              variant="secondary"
              disabled={!canWrite}
              title={!canWrite ? "Needs bridges:write" : undefined}
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
              title={!canWrite ? "Needs bridges:write" : undefined}
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

      {health?.last_error && (
        <div className="mt-4 rounded-md border border-danger-border bg-danger-bg px-4 py-3 text-sm text-danger">
          {health.last_error}
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
              ) : (backlog?.items.length ?? 0) === 0 ? (
                <span className="text-sm text-text">Nothing waiting</span>
              ) : (
                <span className="text-sm text-text">
                  {pendingBacklog.length} pending
                  {deadLettered.length > 0 && (
                    <span className="text-danger">, {deadLettered.length} dead-lettered</span>
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
          </dl>
        </Content>

        <Content value="sign-in" className="py-6">
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
          {backlogIsError ? (
            <QueryProblemState error={backlogError} resource="transactions" />
          ) : (backlog?.items.length ?? 0) === 0 ? (
            <p className="text-sm text-text-muted">No pending or dead-lettered transactions.</p>
          ) : (
            <>
              {deadLettered.length > 0 && (
                <div className="mb-4 flex items-center justify-between rounded-md border border-warning-border bg-warning-bg px-4 py-3">
                  <p className="text-sm text-warning">
                    {deadLettered.length} dead-lettered transaction
                    {deadLettered.length === 1 ? "" : "s"}.
                  </p>
                  <Button
                    variant="secondary"
                    size="sm"
                    disabled={!canWrite}
                    onClick={() =>
                      replay.mutate(id, {
                        onSuccess: (task) =>
                          toast({ title: `Replaying dead letters (task ${task?.id ?? ""})` }),
                        onError: () => toast({ title: "Couldn't start replay", variant: "danger" }),
                      })
                    }
                  >
                    Replay all
                  </Button>
                </div>
              )}
              <ul className="divide-y divide-border rounded-md border border-border">
                {backlog?.items.map((entry) => (
                  <li
                    key={entry.transaction_id}
                    className="flex items-center justify-between gap-3 px-4 py-3"
                  >
                    <span className="font-identifier text-text-muted">{entry.transaction_id}</span>
                    <span className="flex items-center gap-3">
                      {entry.last_error && (
                        <span className="text-xs text-danger">{entry.last_error}</span>
                      )}
                      <span className="text-xs text-text-muted">{entry.attempts} attempt(s)</span>
                      <span className="text-xs text-text-muted">
                        {formatBacklogEntry(entry.age_ms ?? 0, entry.dead_lettered ?? false)}
                      </span>
                      <Badge status={entry.dead_lettered ? "danger" : "info"}>
                        {entry.dead_lettered ? "dead-lettered" : "pending"}
                      </Badge>
                    </span>
                  </li>
                ))}
              </ul>
            </>
          )}
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
                            navigate({ to: "/bridges" });
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

function redactTokens(registration: Record<string, unknown>): Record<string, unknown> {
  const redacted = { ...registration };
  for (const key of ["as_token", "hs_token"]) {
    if (key in redacted) redacted[key] = "•".repeat(24);
  }
  return redacted;
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
