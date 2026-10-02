import { useState, type ReactNode } from "react";
import { useParams, Link } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import {
  useUser,
  useLockUser,
  useUnlockUser,
  useLogoutUser,
  useDeactivateUser,
  useReactivateUser,
} from "@/api/users";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { Dialog, DialogTrigger, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { QueryProblemState } from "@/components/QueryProblemState";
import { CopyableId } from "@/components/CopyableId";
import { RelativeTime } from "@/components/RelativeTime";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import { ResetPasswordDialog } from "./users/ResetPasswordDialog";
import { EditUserDialog } from "./users/EditUserDialog";
import { SendNoticeDialog } from "./settings/SendNoticeDialog";
import { UserDevicesSection } from "./users/UserDevicesSection";
import { UserIdentitySection } from "./users/UserIdentitySection";
import { UserClientDataSection } from "./users/UserClientDataSection";
import { ModerationCard } from "./users/ModerationCard";
import { ActivityCard } from "./users/ActivityCard";

/** `/users/:id` — flows.md flow 2 steps 2-5: understand and act on a user. */
export function UserDetailPage() {
  const { userId } = useParams({ from: "/users/$userId" });
  const { data: user, isLoading, isError, error, refetch } = useUser(userId);
  const canWrite = hasScope("admin:write");
  const canModerate = hasScope("moderation:write");

  const lock = useLockUser();
  const unlock = useUnlockUser();
  const logout = useLogoutUser();
  const deactivate = useDeactivateUser();
  const reactivate = useReactivateUser();
  const [resetOpen, setResetOpen] = useState(false);
  const [editOpen, setEditOpen] = useState(false);
  const [noticeOpen, setNoticeOpen] = useState(false);
  // "Also erase their data" in the deactivate dialog; forgotten when the dialog closes.
  const [eraseToo, setEraseToo] = useState(false);

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

  if (isError || !user) {
    return (
      <div className="p-6">
        <QueryProblemState error={error} resource="this user" onRetry={() => refetch()} />
      </div>
    );
  }

  const id = user.user_id;

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <Link
        to="/users"
        className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
      >
        <ChevronLeft size={14} aria-hidden="true" />
        Users
      </Link>

      <div className="mt-2 flex flex-wrap items-start justify-between gap-4">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-xl text-text">{user.display_name ?? id}</h1>
            {user.admin && (
              <Badge status="info" hideIcon>
                Admin
              </Badge>
            )}
            {user.locked && <Badge status="warning">Locked</Badge>}
            {user.suspended && <Badge status="warning">Suspended</Badge>}
            {user.deactivated && <Badge status="danger">Deactivated</Badge>}
            {user.erased && <Badge status="danger">Erased</Badge>}
            {user.shadow_banned && (
              <Badge status="muted" hideIcon>
                Shadow-banned
              </Badge>
            )}
          </div>
          <p className="mt-1 text-sm text-text-muted">
            <CopyableId value={id} />
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button
            variant="secondary"
            disabled={!canWrite}
            title={
              !canWrite
                ? "Needs admin:write"
                : "Display name, avatar, kind of account, server administrator"
            }
            onClick={() => setEditOpen(true)}
          >
            Edit
          </Button>
          {editOpen && (
            <EditUserDialog key={id} user={user} open={editOpen} onOpenChange={setEditOpen} />
          )}
          {user.locked ? (
            <Button
              variant="secondary"
              disabled={!canModerate}
              title={!canModerate ? "Needs moderation:write" : "Lets them sign in again"}
              onClick={() =>
                unlock.mutate(
                  { userId: id },
                  { onSuccess: () => toast({ title: "User unlocked" }) },
                )
              }
            >
              Unlock
            </Button>
          ) : (
            <Dialog>
              <DialogTrigger asChild>
                <Button variant="secondary" disabled={!canModerate}>
                  Lock
                </Button>
              </DialogTrigger>
              <DialogContent
                title={`Lock ${id}?`}
                description="Blocks sign-in; existing sessions stay active until they sign out or you sign them out separately."
                footer={
                  <>
                    <DialogClose asChild>
                      <Button variant="secondary">Cancel</Button>
                    </DialogClose>
                    <DialogClose asChild>
                      <Button
                        variant="danger"
                        onClick={() =>
                          lock.mutate(
                            { userId: id },
                            { onSuccess: () => toast({ title: "User locked" }) },
                          )
                        }
                      >
                        Lock
                      </Button>
                    </DialogClose>
                  </>
                }
              />
            </Dialog>
          )}
          <Button
            variant="secondary"
            disabled={!canModerate || user.deactivated}
            title={!canModerate ? "Needs moderation:write" : undefined}
            onClick={() => setNoticeOpen(true)}
          >
            Send notice
          </Button>
          <SendNoticeDialog userId={id} open={noticeOpen} onOpenChange={setNoticeOpen} />
          <Button
            variant="secondary"
            disabled={!canWrite || user.erased}
            title={
              !canWrite
                ? "Needs admin:write"
                : user.erased
                  ? "An erased account has no password to reset"
                  : undefined
            }
            onClick={() => setResetOpen(true)}
          >
            Reset password
          </Button>
          <ResetPasswordDialog userId={id} open={resetOpen} onOpenChange={setResetOpen} />
          <Dialog>
            <DialogTrigger asChild>
              <Button variant="secondary" disabled={!canModerate}>
                Sign out everywhere
              </Button>
            </DialogTrigger>
            <DialogContent
              title={`Sign ${id} out everywhere?`}
              description="Every device signs out immediately; they can sign back in unless also locked."
              footer={
                <>
                  <DialogClose asChild>
                    <Button variant="secondary">Cancel</Button>
                  </DialogClose>
                  <DialogClose asChild>
                    <Button
                      variant="danger"
                      onClick={() =>
                        logout.mutate(
                          { userId: id },
                          { onSuccess: () => toast({ title: "Signed out everywhere" }) },
                        )
                      }
                    >
                      Sign out everywhere
                    </Button>
                  </DialogClose>
                </>
              }
            />
          </Dialog>
        </div>
      </div>

      <div className="mt-6 grid grid-cols-1 gap-8 xl:grid-cols-3">
        <div className="xl:col-span-2">
          <h2 className="text-md font-medium text-text">Overview</h2>
          <dl className="mt-3 grid grid-cols-1 gap-x-8 gap-y-4 sm:grid-cols-2">
            <Fact label="Created" value={<RelativeTime at={user.created_at} />} />
            <Fact label="Last seen" value={<RelativeTime at={user.last_seen_at} />} />
            {user.erased && (
              <Fact label="Display name and avatar" value="Cleared when the account was erased" />
            )}
            <Fact label="Rooms" value={String(user.room_count ?? 0)} />
            <Fact label="Media" value={String(user.media_count ?? 0)} />
            <Fact
              label="Kind of account"
              value={
                user.is_guest
                  ? "Guest: no password, and only rooms that let guests in"
                  : (USER_TYPE_LABELS[user.user_type ?? ""] ?? "Person")
              }
            />
            <Fact
              label="Made by a bridge"
              value={
                user.appservice_id ? (
                  <Link
                    to="/bridges/$bridgeId"
                    params={{ bridgeId: user.appservice_id }}
                    className="text-accent hover:underline"
                  >
                    {user.appservice_id}
                  </Link>
                ) : (
                  "No"
                )
              }
            />
          </dl>

          <UserDevicesSection userId={id} canWrite={canWrite} canModerate={canModerate} />
          <UserIdentitySection userId={id} canWrite={canWrite} />
          <UserClientDataSection userId={id} canWrite={canWrite} />

          <div className="mt-8">
            <ActivityCard userId={id} />
          </div>
        </div>

        <div>
          <ModerationCard user={user} />
          <h2 className="mt-8 text-md font-medium text-text">Danger</h2>
          {user.erased && (
            <div className="mt-3 rounded-md border border-border bg-surface p-4">
              <h3 className="text-sm font-medium text-text">This account was erased</h3>
              <p className="mt-1 text-sm text-text-muted">
                Nothing personal is left on the server, and it cannot be reactivated: erasure cannot
                be undone.
              </p>
              <EraseExplanation />
            </div>
          )}
          {user.deactivated && !user.erased && (
            <div className="mt-3 rounded-md border border-border bg-surface p-4">
              <h3 className="text-sm font-medium text-text">Reactivate this user</h3>
              <p className="mt-1 text-sm text-text-muted">
                Lets them sign in again. Rooms they were taken out of when deactivated are not
                rejoined; they can be invited back.
              </p>
              <Button
                variant="secondary"
                className="mt-3"
                disabled={!canWrite || reactivate.isPending}
                title={!canWrite ? "Needs admin:write" : undefined}
                onClick={() =>
                  reactivate.mutate(
                    { userId: id },
                    { onSuccess: () => toast({ title: `User ${id} reactivated` }) },
                  )
                }
              >
                Reactivate
              </Button>
            </div>
          )}
          {user.deactivated && !user.erased && (
            <div className="mt-3 rounded-md border border-danger-border bg-danger-bg p-4">
              <h3 className="text-sm font-medium text-text">Erase this user's data</h3>
              <p className="mt-1 text-sm text-text-muted">
                Removes everything personal the server still holds about this deactivated account.
                It cannot be undone, and the account can never be reactivated.
              </p>
              <Dialog>
                <DialogTrigger asChild>
                  <Button
                    variant="danger"
                    className="mt-3"
                    disabled={!canWrite}
                    title={!canWrite ? "Needs admin:write" : undefined}
                  >
                    Erase data
                  </Button>
                </DialogTrigger>
                <DialogContent
                  title={`Erase ${id}'s data?`}
                  description="This cannot be undone, and the account cannot be reactivated afterwards."
                  footer={
                    <>
                      <DialogClose asChild>
                        <Button variant="secondary">Cancel</Button>
                      </DialogClose>
                      <DialogClose asChild>
                        <Button
                          variant="danger"
                          onClick={() =>
                            deactivate.mutate(
                              { userId: id, erase: true },
                              {
                                onSuccess: () => toast({ title: `User ${id} erased` }),
                                onError: () =>
                                  toast({ title: `Couldn't erase ${id}`, variant: "danger" }),
                              },
                            )
                          }
                        >
                          Erase data
                        </Button>
                      </DialogClose>
                    </>
                  }
                >
                  <EraseExplanation />
                </DialogContent>
              </Dialog>
            </div>
          )}
          <div
            className="mt-3 rounded-md border border-danger-border bg-danger-bg p-4"
            hidden={user.deactivated}
          >
            <h3 className="text-sm font-medium text-text">Deactivate this user</h3>
            <p className="mt-1 text-sm text-text-muted">
              They can no longer sign in, and are signed out everywhere. Their messages stay where
              they are; redact them under Moderation if they must go. You can also erase their data
              at the same time.
            </p>
            <Dialog onOpenChange={(open) => !open && setEraseToo(false)}>
              <DialogTrigger asChild>
                <Button variant="danger" className="mt-3" disabled={!canWrite}>
                  Deactivate
                </Button>
              </DialogTrigger>
              <DialogContent
                title={`Deactivate ${id}?`}
                description="They are signed out everywhere and can no longer sign in. Their messages stay unless you redact them. Reactivate on this page lets them sign in again, unless you also erase their data."
                footer={
                  <>
                    <DialogClose asChild>
                      <Button variant="secondary">Cancel</Button>
                    </DialogClose>
                    <DialogClose asChild>
                      <Button
                        variant="danger"
                        onClick={() =>
                          deactivate.mutate(
                            { userId: id, erase: eraseToo || undefined },
                            {
                              onSuccess: () =>
                                toast({
                                  title: eraseToo
                                    ? `User ${id} deactivated and erased`
                                    : `User ${id} deactivated`,
                                }),
                              onError: () =>
                                toast({ title: `Couldn't deactivate ${id}`, variant: "danger" }),
                            },
                          )
                        }
                      >
                        {eraseToo ? "Deactivate and erase" : "Deactivate"}
                      </Button>
                    </DialogClose>
                  </>
                }
              >
                <label className="flex items-start gap-2 text-sm text-text">
                  <input
                    type="checkbox"
                    className="mt-1 size-4 accent-[var(--color-accent)]"
                    checked={eraseToo}
                    onChange={(e) => setEraseToo(e.target.checked)}
                  />
                  <span>
                    Also erase their data
                    <span className="block text-xs text-text-muted">
                      Leave this off to keep the option of reactivating them later.
                    </span>
                  </span>
                </label>
                {eraseToo && <EraseExplanation />}
              </DialogContent>
            </Dialog>
          </div>
        </div>
      </div>
    </div>
  );
}

/** `User.user_type`, in words: the Matrix types an account can have. */
const USER_TYPE_LABELS: Record<string, string> = {
  bot: "Bot",
  support: "Support account",
};

/**
 * What erasing an account does, in the words the deactivate dialog, the erase box and the
 * erased account's page all use. Erasure is the GDPR-style "forget me": it goes further than
 * deactivation and, unlike it, cannot be undone.
 */
function EraseExplanation() {
  return (
    <div className="mt-3 text-sm text-text-muted" data-testid="erase-explanation">
      <p>Erasing removes what the server holds about them:</p>
      <ul className="mt-1 list-disc space-y-0.5 pl-5">
        <li>their password, and every session is signed out</li>
        <li>every device, with its encryption keys</li>
        <li>their email addresses, phone numbers and single-sign-on links</li>
        <li>their display name and avatar</li>
        <li>their membership of every room: the account leaves them all</li>
      </ul>
      <p className="mt-2">
        What stays: the messages they sent, in the rooms they sent them to, unless you redact them
        under Moderation. The account cannot be reactivated and the erasure cannot be undone.
      </p>
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
