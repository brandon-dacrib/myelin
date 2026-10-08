import { useState, type ReactNode } from "react";
import type { User } from "@/api/users";
import { useSuspendUser, useUnsuspendUser } from "@/api/users";
import {
  redactResult,
  useShadowBanUser,
  useUnshadowBanUser,
  userMediaDeleteResult,
} from "@/api/user-moderation";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { MutationError } from "@/components/MutationError";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import { formatBytes } from "@/lib/format";
import { ReasonConfirmDialog } from "./ReasonConfirmDialog";
import { RateLimitSection } from "./RateLimitSection";
import { LoginAsDialog } from "./LoginAsDialog";
import { RedactEventsDialog } from "./RedactEventsDialog";
import { DeleteUserMediaDialog } from "./DeleteUserMediaDialog";
import { FollowTask } from "./FollowTask";

type Open = "suspend" | "shadow-ban" | "login-as" | "redact" | "media" | null;

/**
 * What an administrator can do about a user's behaviour, gathered in one place: suspend,
 * shadow-ban, a message rate limit, support sign-in, and cleaning up what they sent and
 * uploaded. Each control is disabled, with the scope it needs, for a session that lacks it.
 */
export function ModerationCard({ user }: { user: User }) {
  const id = user.user_id;
  const canWrite = hasScope("admin:write");
  const canModerate = hasScope("moderation:write");
  const [open, setOpen] = useState<Open>(null);
  const [redactTaskId, setRedactTaskId] = useState<string | null>(null);
  const [mediaTaskId, setMediaTaskId] = useState<string | null>(null);

  const suspend = useSuspendUser();
  const unsuspend = useUnsuspendUser();
  const shadowBan = useShadowBanUser();
  const unshadowBan = useUnshadowBanUser();

  const close = () => setOpen(null);

  return (
    <section aria-labelledby="moderation-heading">
      <h2 id="moderation-heading" className="text-md font-medium text-text">
        Moderation
      </h2>
      <div className="mt-3 flex flex-col divide-y divide-border rounded-md border border-border">
        <Block
          title="Suspension"
          status={
            user.suspended ? (
              <Badge status="warning">Suspended</Badge>
            ) : (
              <Badge status="success">Not suspended</Badge>
            )
          }
          text={
            user.suspended
              ? "They can read, leave rooms, redact their own messages and sign out. Everything else they try is refused."
              : "Suspending lets them read but stops them sending, joining or changing anything until you lift it."
          }
        >
          {user.suspended ? (
            <Button
              variant="secondary"
              size="sm"
              disabled={!canModerate || unsuspend.isPending}
              title={!canModerate ? "Needs moderation:write" : undefined}
              onClick={() =>
                unsuspend.mutate(
                  { userId: id },
                  { onSuccess: () => toast({ title: "User unsuspended" }) },
                )
              }
            >
              Unsuspend
            </Button>
          ) : (
            <Button
              variant="secondary"
              size="sm"
              disabled={!canModerate}
              title={!canModerate ? "Needs moderation:write" : undefined}
              onClick={() => {
                suspend.reset();
                setOpen("suspend");
              }}
            >
              Suspend
            </Button>
          )}
          {unsuspend.error != null && (
            <MutationError error={unsuspend.error} action="unsuspend them" className="mt-2" />
          )}
        </Block>

        <Block
          title="Shadow-ban"
          status={
            user.shadow_banned ? (
              <Badge status="muted" hideIcon>
                Shadow-banned
              </Badge>
            ) : null
          }
          text={
            user.shadow_banned
              ? "What they send and the invites they make look sent to them, but reach nobody."
              : "Their messages and invites would look sent to them but reach nobody. They are not told."
          }
        >
          {user.shadow_banned ? (
            <Button
              variant="secondary"
              size="sm"
              disabled={!canModerate || unshadowBan.isPending}
              title={!canModerate ? "Needs moderation:write" : undefined}
              onClick={() =>
                unshadowBan.mutate(
                  { userId: id },
                  { onSuccess: () => toast({ title: "Shadow-ban lifted" }) },
                )
              }
            >
              Lift shadow-ban
            </Button>
          ) : (
            <Button
              variant="secondary"
              size="sm"
              disabled={!canModerate}
              title={!canModerate ? "Needs moderation:write" : undefined}
              onClick={() => {
                shadowBan.reset();
                setOpen("shadow-ban");
              }}
            >
              Shadow-ban
            </Button>
          )}
          {unshadowBan.error != null && (
            <MutationError
              error={unshadowBan.error}
              action="lift the shadow-ban"
              className="mt-2"
            />
          )}
        </Block>

        <div className="p-4">
          <RateLimitSection userId={id} admin={user.admin} appserviceId={user.appservice_id} />
        </div>

        <Block
          title="Support access"
          text="Create a short-lived token that acts as this user, to see what they see. It is audited and shows in their sessions."
        >
          <Button
            variant="secondary"
            size="sm"
            disabled={!canWrite || user.deactivated}
            title={
              !canWrite
                ? "Needs admin:write"
                : user.deactivated
                  ? "A deactivated account cannot be signed in as"
                  : undefined
            }
            onClick={() => setOpen("login-as")}
          >
            Sign in as user…
          </Button>
        </Block>

        <Block
          title="Clean up"
          text="Remove what they sent and uploaded. Both run as tasks on the server and cannot be undone."
        >
          <div className="flex flex-wrap gap-2">
            <Button
              variant="danger"
              size="sm"
              disabled={!canModerate}
              title={!canModerate ? "Needs moderation:write" : undefined}
              onClick={() => setOpen("redact")}
            >
              Redact messages…
            </Button>
            <Button
              variant="danger"
              size="sm"
              disabled={!canModerate}
              title={!canModerate ? "Needs moderation:write" : undefined}
              onClick={() => setOpen("media")}
            >
              Delete all media…
            </Button>
          </div>
          {(redactTaskId || mediaTaskId) && (
            <div className="mt-3 flex flex-col gap-2">
              {redactTaskId && (
                <FollowTask
                  key={redactTaskId}
                  taskId={redactTaskId}
                  title="Redacting messages"
                  result={(task) => {
                    const r = redactResult(task);
                    return (
                      <>
                        <p>
                          Redacted {r.redacted.toLocaleString()} of {r.total.toLocaleString()}{" "}
                          {r.total === 1 ? "event" : "events"}.
                        </p>
                        {r.failed_count > 0 && (
                          <details className="mt-1">
                            <summary className="cursor-pointer text-danger">
                              {r.failed_count.toLocaleString()} could not be redacted
                            </summary>
                            <ul className="mt-1 list-disc pl-5 text-xs text-text-muted">
                              {r.failed.map((f, i) => (
                                <li key={f.event_id ?? i}>
                                  <span className="font-identifier">{f.event_id ?? "?"}</span>
                                  {f.room_id && (
                                    <>
                                      {" in "}
                                      <span className="font-identifier">{f.room_id}</span>
                                    </>
                                  )}
                                  {(f.reason ?? f.error) && `: ${f.reason ?? f.error}`}
                                </li>
                              ))}
                            </ul>
                          </details>
                        )}
                      </>
                    );
                  }}
                />
              )}
              {mediaTaskId && (
                <FollowTask
                  key={mediaTaskId}
                  taskId={mediaTaskId}
                  title="Deleting media"
                  result={(task) => {
                    const r = userMediaDeleteResult(task);
                    return (
                      <p>
                        Deleted {r.deleted.toLocaleString()} {r.deleted === 1 ? "file" : "files"} (
                        {formatBytes(r.bytes)}).
                        {r.skipped_protected > 0 &&
                          ` Kept ${r.skipped_protected.toLocaleString()} protected.`}
                        {r.failed > 0 && ` ${r.failed.toLocaleString()} could not be deleted.`}
                      </p>
                    );
                  }}
                />
              )}
            </div>
          )}
        </Block>
      </div>

      <ReasonConfirmDialog
        open={open === "suspend"}
        onOpenChange={(next) => !next && close()}
        title={`Suspend ${id}?`}
        description="They can still read, leave rooms, redact their own messages and sign out. Everything else is refused until you unsuspend them."
        confirmLabel="Suspend"
        pendingLabel="Suspending…"
        action="suspend them"
        pending={suspend.isPending}
        error={suspend.error}
        onConfirm={(reason) =>
          suspend.mutate(
            { userId: id, reason: reason || undefined },
            {
              onSuccess: () => {
                close();
                toast({
                  title: "User suspended",
                  action: { label: "Undo", onClick: () => unsuspend.mutate({ userId: id }) },
                });
              },
            },
          )
        }
      />
      <ReasonConfirmDialog
        open={open === "shadow-ban"}
        onOpenChange={(next) => !next && close()}
        title={`Shadow-ban ${id}?`}
        description="Their messages and invites will look sent to them but reach nobody. They are not told."
        confirmLabel="Shadow-ban"
        pendingLabel="Shadow-banning…"
        action="shadow-ban them"
        pending={shadowBan.isPending}
        error={shadowBan.error}
        onConfirm={(reason) =>
          shadowBan.mutate(
            { userId: id, reason: reason || undefined },
            {
              onSuccess: () => {
                close();
                toast({ title: "User shadow-banned" });
              },
            },
          )
        }
      />
      <LoginAsDialog
        userId={id}
        open={open === "login-as"}
        onOpenChange={(next) => !next && close()}
      />
      <RedactEventsDialog
        userId={id}
        open={open === "redact"}
        onOpenChange={(next) => !next && close()}
        onStarted={(task) => setRedactTaskId(task.id)}
      />
      <DeleteUserMediaDialog
        userId={id}
        open={open === "media"}
        onOpenChange={(next) => !next && close()}
        onStarted={(task) => setMediaTaskId(task.id)}
      />
    </section>
  );
}

function Block({
  title,
  status,
  text,
  children,
}: {
  title: string;
  status?: ReactNode;
  text: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="p-4">
      <div className="flex flex-wrap items-center gap-2">
        <h3 className="text-sm font-medium text-text">{title}</h3>
        {status}
      </div>
      <p className="mt-1 text-sm text-text-muted">{text}</p>
      <div className="mt-3">{children}</div>
    </div>
  );
}
