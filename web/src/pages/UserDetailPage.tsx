import { useState, type ReactNode } from "react";
import { useParams, Link } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import { useUser, useLockUser, useUnlockUser, useLogoutUser, useDeactivateUser } from "@/api/users";
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
  const [resetOpen, setResetOpen] = useState(false);
  const [noticeOpen, setNoticeOpen] = useState(false);

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
          {user.locked ? (
            <Button
              variant="secondary"
              disabled={!canModerate}
              title={!canModerate ? "Needs moderation:write" : undefined}
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
          <Button variant="secondary" disabled={!canWrite} onClick={() => setResetOpen(true)}>
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
            <Fact label="Rooms" value={String(user.room_count ?? 0)} />
            <Fact label="Media" value={String(user.media_count ?? 0)} />
            <Fact label="User type" value={user.user_type ?? "person"} />
            <Fact label="Appservice" value={user.appservice_id ?? "—"} />
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
          <div className="mt-3 rounded-md border border-danger-border bg-danger-bg p-4">
            <h3 className="text-sm font-medium text-text">Deactivate this user</h3>
            <p className="mt-1 text-sm text-text-muted">
              They can no longer sign in. Their messages stay unless you also erase.
            </p>
            <Dialog>
              <DialogTrigger asChild>
                <Button variant="danger" className="mt-3" disabled={!canWrite}>
                  Deactivate
                </Button>
              </DialogTrigger>
              <DialogContent
                title={`Deactivate ${id}?`}
                description="This cannot be undone from here. Their messages and rooms stay unless you separately redact or erase."
                footer={
                  <>
                    <DialogClose asChild>
                      <Button variant="secondary">Cancel</Button>
                    </DialogClose>
                    <Button
                      variant="danger"
                      onClick={() =>
                        deactivate.mutate(
                          { userId: id },
                          { onSuccess: () => toast({ title: `User ${id} deactivated` }) },
                        )
                      }
                    >
                      Deactivate
                    </Button>
                  </>
                }
              />
            </Dialog>
          </div>
        </div>
      </div>
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
