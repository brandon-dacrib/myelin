import type { ReactNode } from "react";
import { useParams, Link } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import {
  useRoom,
  useRoomMembers,
  useBlockRoom,
  useUnblockRoom,
  useMakeRoomAdmin,
} from "@/api/rooms";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { Dialog, DialogTrigger, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { QueryProblemState } from "@/components/QueryProblemState";
import { CopyableId } from "@/components/CopyableId";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";

/** `/rooms/:id` — flows.md flow 3 steps 2-7: understand and act on a room. */
export function RoomDetailPage() {
  const { roomId } = useParams({ from: "/rooms/$roomId" });
  const { data: room, isLoading, isError, error, refetch } = useRoom(roomId);
  const {
    data: members,
    isError: membersIsError,
    error: membersError,
    refetch: refetchMembers,
  } = useRoomMembers(roomId);
  const canModerate = hasScope("moderation:write");
  const canWrite = hasScope("admin:write");

  const block = useBlockRoom();
  const unblock = useUnblockRoom();
  const makeAdmin = useMakeRoomAdmin();

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

  if (isError || !room) {
    return (
      <div className="p-6">
        <QueryProblemState error={error} resource="this room" onRetry={() => refetch()} />
      </div>
    );
  }

  const id = room.room_id;

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <Link
        to="/rooms"
        className="inline-flex items-center gap-1 text-sm text-text-muted hover:text-text"
      >
        <ChevronLeft size={14} aria-hidden="true" />
        Rooms
      </Link>

      <div className="mt-2 flex flex-wrap items-start justify-between gap-4">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <h1 className="text-xl text-text">{room.name ?? room.canonical_alias ?? id}</h1>
            {room.public && (
              <Badge status="info" hideIcon>
                Public
              </Badge>
            )}
            {room.encrypted && (
              <Badge status="muted" hideIcon>
                Encrypted
              </Badge>
            )}
            {room.blocked && <Badge status="danger">Blocked</Badge>}
          </div>
          <p className="mt-1 text-sm text-text-muted">
            <CopyableId value={id} />
            {room.canonical_alias && ` · ${room.canonical_alias}`}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <Button
            variant="secondary"
            disabled={!canWrite}
            title={!canWrite ? "Needs admin:write" : undefined}
            onClick={() =>
              makeAdmin.mutate(id, { onSuccess: () => toast({ title: "Joined as admin" }) })
            }
          >
            Make me admin
          </Button>
          {room.blocked ? (
            <Button
              variant="secondary"
              disabled={!canModerate}
              onClick={() =>
                unblock.mutate(id, { onSuccess: () => toast({ title: "Room unblocked" }) })
              }
            >
              Unblock
            </Button>
          ) : (
            <Dialog>
              <DialogTrigger asChild>
                <Button variant="danger" disabled={!canModerate}>
                  Block
                </Button>
              </DialogTrigger>
              <DialogContent
                title={`Block ${room.name ?? id}?`}
                description="Prevents new joins; current members remain and can still send messages."
                footer={
                  <>
                    <DialogClose asChild>
                      <Button variant="secondary">Cancel</Button>
                    </DialogClose>
                    <DialogClose asChild>
                      <Button
                        variant="danger"
                        onClick={() =>
                          block.mutate(
                            { roomId: id },
                            { onSuccess: () => toast({ title: "Room blocked" }) },
                          )
                        }
                      >
                        Block
                      </Button>
                    </DialogClose>
                  </>
                }
              />
            </Dialog>
          )}
        </div>
      </div>

      <div className="mt-6 grid grid-cols-1 gap-8 xl:grid-cols-3">
        <div className="xl:col-span-2">
          <h2 className="text-md font-medium text-text">Overview</h2>
          <dl className="mt-3 grid grid-cols-1 gap-x-8 gap-y-4 sm:grid-cols-2">
            <Fact label="Creator" value={room.creator ?? "—"} />
            <Fact label="Version" value={room.version ?? "—"} />
            <Fact label="Topic" value={room.topic ?? "—"} />
            <Fact label="Join rule" value={room.join_rule ?? "—"} />
            <Fact label="History visibility" value={room.history_visibility ?? "—"} />
            <Fact
              label="Members (local / joined)"
              value={`${room.local_members_count ?? 0} / ${room.joined_members_count ?? 0}`}
            />
            <Fact label="State events" value={String(room.state_events_count ?? 0)} />
            <Fact label="Federatable" value={room.federatable ? "Yes" : "No"} />
          </dl>

          <h2 className="mt-8 text-md font-medium text-text">Members</h2>
          {membersIsError ? (
            <QueryProblemState
              error={membersError}
              resource="this room's members"
              onRetry={() => refetchMembers()}
            />
          ) : (members?.items.length ?? 0) === 0 ? (
            <p className="mt-3 text-sm text-text-muted">No members loaded.</p>
          ) : (
            <ul className="mt-3 divide-y divide-border rounded-md border border-border">
              {members?.items.map((m) => (
                <li key={m.user_id} className="flex items-center justify-between gap-3 px-4 py-3">
                  <span className="font-identifier text-text">{m.user_id}</span>
                  <Badge status="neutral" hideIcon>
                    {m.membership}
                  </Badge>
                </li>
              ))}
            </ul>
          )}
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
