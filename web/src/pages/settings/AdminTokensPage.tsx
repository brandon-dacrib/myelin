import { useState } from "react";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { KeyRound, Plus, Trash2 } from "lucide-react";
import { useAdminTokens, useRevokeAdminToken, type AdminTokenView } from "@/api/admin-tokens";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { toast } from "@/components/ui/toast/toast-store";
import { QueryProblemState } from "@/components/QueryProblemState";
import { CopyableId } from "@/components/CopyableId";
import { RelativeTime } from "@/components/RelativeTime";
import { hasScope } from "@/lib/auth";
import { formatExpiry } from "@/lib/registration-tokens";
import { SCOPE_DESCRIPTIONS, describeScopes } from "@/lib/scopes";
import { SettingsTabs } from "./SettingsTabs";
import { CreateAdminTokenDialog } from "./CreateAdminTokenDialog";

/** The scopes a token holds, one badge each, with the sentence for each on hover. */
export function ScopeBadges({ scopes }: { scopes: AdminTokenView["scopes"] }) {
  const full = scopes.includes("admin:write");
  return (
    <span className="flex flex-wrap gap-1">
      {(full ? (["admin:write"] as const) : scopes).map((scope) => (
        <span key={scope} title={SCOPE_DESCRIPTIONS.find((d) => d.scope === scope)?.grants}>
          <Badge
            status={scope.endsWith(":write") ? "warning" : "neutral"}
            hideIcon
            className="font-identifier"
          >
            {scope}
          </Badge>
        </span>
      ))}
    </span>
  );
}

function expired(t: AdminTokenView, nowMs = Date.now()): boolean {
  return t.expiresAt !== null && Date.parse(t.expiresAt) <= nowMs;
}

/**
 * `/settings/admin-tokens`: the admin API tokens minted on this server, each with the scopes it
 * carries. Mint one with a chosen set of scopes, or revoke one.
 */
export function AdminTokensPage() {
  const search = useSearch({ from: "/settings/admin-tokens" });
  const navigate = useNavigate({ from: "/settings/admin-tokens" });
  const canRead = hasScope("admin:read");
  const canWrite = hasScope("admin:write");
  const [createOpen, setCreateOpen] = useState(false);
  const [revoking, setRevoking] = useState<AdminTokenView | null>(null);
  const revoke = useRevokeAdminToken();
  const { data, isLoading, isError, error, refetch } = useAdminTokens(search.cursor);

  if (!canRead) {
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Invites and tokens</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );
  }

  const columns: Column<AdminTokenView>[] = [
    {
      key: "name",
      header: "Name",
      priority: 1,
      render: (t) => (
        <span className="flex flex-col gap-0.5">
          <span className="text-text">{t.name}</span>
          <span className="text-xs text-text-muted">
            <CopyableId value={t.id} label={`id of ${t.name}`} />
          </span>
        </span>
      ),
      renderCompact: (t) => t.name,
      interactive: true,
    },
    {
      key: "scopes",
      header: "Scopes",
      priority: 1,
      render: (t) => <ScopeBadges scopes={t.scopes} />,
      renderCompact: (t) => describeScopes(t.scopes),
    },
    {
      key: "expires",
      header: "Expires",
      priority: 2,
      render: (t) =>
        t.expiresAt ? (
          <span className="flex flex-col items-start gap-0.5">
            <time dateTime={t.expiresAt} title={new Date(t.expiresAt).toISOString()}>
              {formatExpiry(t.expiresAt)}
            </time>
            {expired(t) && <Badge status="muted">Expired</Badge>}
          </span>
        ) : (
          <span className="text-text-muted">Never</span>
        ),
      renderCompact: (t) => formatExpiry(t.expiresAt),
    },
    {
      key: "created",
      header: "Minted",
      priority: 3,
      render: (t) => (
        <span className="flex flex-col gap-0.5">
          <RelativeTime at={t.createdAt} />
          <span className="text-xs text-text-muted">by {t.createdBy}</span>
        </span>
      ),
    },
    {
      key: "actions",
      header: "Actions",
      priority: 1,
      interactive: true,
      render: (t) =>
        canWrite ? (
          <div className="flex justify-end">
            <Button
              variant="ghost"
              size="sm"
              aria-label={`Revoke ${t.name}`}
              leadingIcon={<Trash2 size={14} aria-hidden="true" />}
              onClick={() => setRevoking(t)}
            >
              Revoke
            </Button>
          </div>
        ) : null,
    },
  ];

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <h1 className="text-xl text-text">Invites and tokens</h1>
      <SettingsTabs current="admin-tokens" />

      <div className="mt-6 flex flex-wrap items-start justify-between gap-3">
        <div className="max-w-2xl">
          <h2 className="text-md font-medium text-text">API tokens</h2>
          <p className="mt-1 text-sm text-text-muted">
            A token lets a script, a bot or another team use this server&apos;s admin API with only
            the permissions (scopes) it carries; a request outside them is refused, and the refusal
            names the scope. Each token is shown once when it is minted; this list shows what it can
            do, never the token itself. Signing in here with your own account still gives every
            scope.
          </p>
        </div>
        {canWrite && (
          <Button
            leadingIcon={<Plus size={16} aria-hidden="true" />}
            onClick={() => setCreateOpen(true)}
          >
            Mint token
          </Button>
        )}
      </div>
      <CreateAdminTokenDialog open={createOpen} onOpenChange={setCreateOpen} />
      <Dialog open={revoking !== null} onOpenChange={(open) => !open && setRevoking(null)}>
        {revoking && (
          <DialogContent
            title={`Revoke ${revoking.name}?`}
            description="Whatever uses this token stops working at its next request. The audit log keeps the record of what it was minted with and what it did. Mint a new token to replace it."
            footer={
              <>
                <DialogClose asChild>
                  <Button variant="secondary">Cancel</Button>
                </DialogClose>
                <Button
                  variant="danger"
                  disabled={revoke.isPending}
                  onClick={() => {
                    const { id, name } = revoking;
                    revoke.mutate(id, {
                      onSuccess: () => {
                        toast({ title: `Token ${name} revoked` });
                        setRevoking(null);
                      },
                      onError: () => {
                        toast({ title: `Couldn’t revoke ${name}`, variant: "danger" });
                        setRevoking(null);
                      },
                    });
                  }}
                >
                  Revoke token
                </Button>
              </>
            }
          />
        )}
      </Dialog>

      {isError ? (
        <div className="mt-6">
          <QueryProblemState
            error={error}
            resource="admin tokens"
            scope="admin:read"
            onRetry={() => refetch()}
          />
        </div>
      ) : (
        <div className="mt-4">
          <DataTable
            caption="API tokens"
            columns={columns}
            rows={data?.items ?? []}
            getRowId={(t) => t.id}
            loading={isLoading}
            empty={
              <EmptyState
                icon={<KeyRound aria-hidden="true" />}
                title="No admin tokens"
                description="Mint one to give a script, a bot or a team part of the admin API, with the scopes you choose."
                action={
                  canWrite ? (
                    <Button onClick={() => setCreateOpen(true)}>Mint the first token</Button>
                  ) : undefined
                }
              />
            }
            pagination={
              data?.nextCursor || search.cursor
                ? {
                    hasPrevious: Boolean(search.cursor),
                    hasNext: Boolean(data?.nextCursor),
                    onPrevious: () => navigate({ search: {} }),
                    onNext: () =>
                      data?.nextCursor && navigate({ search: { cursor: data.nextCursor } }),
                  }
                : undefined
            }
          />
        </div>
      )}
    </div>
  );
}
