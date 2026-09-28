import { useState } from "react";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { KeyRound, Link2, Pencil, Trash2 } from "lucide-react";
import { useDeleteRegistrationToken, useRegistrationTokens } from "@/api/registration-tokens";
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
import {
  formatExpiry,
  formatUses,
  inviteLink,
  tokenStatus,
  type RegistrationTokenView,
  type TokenStatusKind,
} from "@/lib/registration-tokens";
import { SettingsTabs } from "./SettingsTabs";
import { CreateTokenDialog } from "./CreateTokenDialog";
import { EditTokenDialog } from "./EditTokenDialog";
import { CopyInviteLinkButton } from "./token-controls";

const BADGE_FOR: Record<TokenStatusKind, "success" | "muted" | "warning" | "danger"> = {
  valid: "success",
  expired: "muted",
  "used-up": "muted",
  reserved: "warning",
  invalid: "danger",
};

/**
 * `/settings/registration-tokens`: the tokens that let somebody register while registration is
 * closed, each one an invite link. Make one, copy its link, change its limits, or delete it.
 */
export function RegistrationTokensPage() {
  const search = useSearch({ from: "/settings/registration-tokens" });
  const navigate = useNavigate({ from: "/settings/registration-tokens" });
  const canRead = hasScope("admin:read");
  const canWrite = hasScope("admin:write");
  const [createOpen, setCreateOpen] = useState(false);
  const [editing, setEditing] = useState<RegistrationTokenView | null>(null);
  const [deleting, setDeleting] = useState<RegistrationTokenView | null>(null);
  const remove = useDeleteRegistrationToken();
  const { data, isLoading, isError, error, refetch } = useRegistrationTokens(search.cursor);

  if (!canRead) {
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Settings</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );
  }

  const columns: Column<RegistrationTokenView>[] = [
    {
      key: "token",
      header: "Token",
      priority: 1,
      interactive: true,
      render: (t) => <CopyableId value={t.token} />,
      renderCompact: (t) => t.token,
    },
    {
      key: "status",
      header: "Status",
      priority: 1,
      render: (t) => {
        const status = tokenStatus(t);
        return (
          <span className="flex flex-col items-start gap-0.5">
            <Badge status={BADGE_FOR[status.kind]}>{status.label}</Badge>
            {status.detail && <span className="text-xs text-text-muted">{status.detail}</span>}
          </span>
        );
      },
      renderCompact: (t) => tokenStatus(t).label,
    },
    {
      key: "uses",
      header: "Uses",
      priority: 1,
      render: (t) => <span className="tabular-nums">{formatUses(t)}</span>,
      renderCompact: (t) => formatUses(t),
    },
    {
      key: "pending",
      header: "Pending",
      priority: 3,
      align: "end",
      render: (t) => t.pending,
    },
    {
      key: "expires",
      header: "Expires",
      priority: 2,
      render: (t) =>
        t.expiresAt ? (
          <time dateTime={t.expiresAt} title={new Date(t.expiresAt).toISOString()}>
            {formatExpiry(t.expiresAt)}
          </time>
        ) : (
          <span className="text-text-muted">Never</span>
        ),
      renderCompact: (t) => formatExpiry(t.expiresAt),
    },
    {
      key: "created",
      header: "Created",
      priority: 3,
      render: (t) => <RelativeTime at={t.createdAt} />,
    },
    {
      key: "actions",
      header: "Actions",
      priority: 1,
      interactive: true,
      render: (t) => (
        <div className="flex flex-wrap justify-end gap-1">
          <CopyInviteLinkButton link={inviteLink(t.token)} token={t.token} />
          {canWrite && (
            <>
              <Button
                variant="ghost"
                size="sm"
                aria-label={`Edit ${t.token}`}
                leadingIcon={<Pencil size={14} aria-hidden="true" />}
                onClick={() => setEditing(t)}
              >
                Edit
              </Button>
              <Button
                variant="ghost"
                size="sm"
                aria-label={`Delete ${t.token}`}
                leadingIcon={<Trash2 size={14} aria-hidden="true" />}
                onClick={() => setDeleting(t)}
              >
                Delete
              </Button>
            </>
          )}
        </div>
      ),
    },
  ];

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <h1 className="text-xl text-text">Settings</h1>
      <SettingsTabs current="registration-tokens" />

      <div className="mt-6 flex flex-wrap items-start justify-between gap-3">
        <div className="max-w-2xl">
          <h2 className="text-md font-medium text-text">Registration tokens</h2>
          <p className="mt-1 text-sm text-text-muted">
            A token lets somebody create an account on this server while registration is closed.
            Each one is an invite link: send it, and the person picks their own username and
            password.
          </p>
        </div>
        {canWrite && (
          <Button
            leadingIcon={<Link2 size={16} aria-hidden="true" />}
            onClick={() => setCreateOpen(true)}
          >
            Create invite link
          </Button>
        )}
      </div>
      <CreateTokenDialog open={createOpen} onOpenChange={setCreateOpen} />
      {editing && (
        <EditTokenDialog key={editing.token} token={editing} onClose={() => setEditing(null)} />
      )}
      <Dialog open={deleting !== null} onOpenChange={(open) => !open && setDeleting(null)}>
        {deleting && (
          <DialogContent
            title={`Delete ${deleting.token}?`}
            description="Its invite link stops working, and the record of how many accounts it made goes with it. Accounts already created stay. To stop the link but keep the record, edit it and choose Expire now."
            footer={
              <>
                <DialogClose asChild>
                  <Button variant="secondary">Cancel</Button>
                </DialogClose>
                <Button
                  variant="danger"
                  disabled={remove.isPending}
                  onClick={() => {
                    const token = deleting.token;
                    remove.mutate(token, {
                      onSuccess: () => {
                        toast({ title: `Token ${token} deleted` });
                        setDeleting(null);
                      },
                      onError: () => {
                        toast({ title: `Couldn’t delete ${token}`, variant: "danger" });
                        setDeleting(null);
                      },
                    });
                  }}
                >
                  Delete token
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
            resource="registration tokens"
            scope="admin:read"
            onRetry={() => refetch()}
          />
        </div>
      ) : (
        <div className="mt-4">
          <DataTable
            caption="Registration tokens"
            columns={columns}
            rows={data?.items ?? []}
            getRowId={(t) => t.token}
            loading={isLoading}
            empty={
              <EmptyState
                icon={<KeyRound aria-hidden="true" />}
                title="No registration tokens"
                description="Create an invite link to let somebody register while registration is closed."
                action={
                  canWrite ? (
                    <Button onClick={() => setCreateOpen(true)}>Create invite link</Button>
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
