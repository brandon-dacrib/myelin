import {
  HISTORY_VISIBILITY_LABELS,
  JOIN_RULE_LABELS,
  MEMBERSHIP_LABELS,
  roomWords,
} from "@/lib/rooms";
import { useState, type ReactNode } from "react";
import { useParams, useSearch, useNavigate, Link } from "@tanstack/react-router";
import { ChevronLeft } from "lucide-react";
import {
  useRoom,
  useRoomMembers,
  useBlockRoom,
  useUnblockRoom,
  useMakeRoomAdmin,
  type Room,
} from "@/api/rooms";
import { useRoomHierarchy } from "@/api/room-contents";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { Dialog, DialogTrigger, DialogClose, DialogContent } from "@/components/ui/dialog/Dialog";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { QueryProblemState } from "@/components/QueryProblemState";
import { CopyableId } from "@/components/CopyableId";
import { toast } from "@/components/ui/toast/toast-store";
import { hasScope } from "@/lib/auth";
import { cn } from "@/lib/cn";
import { ROOM_TABS, type RoomTab } from "@/pages/rooms/room-search";
import { RoomStateTab } from "@/pages/rooms/RoomStateTab";
import { RoomTimelineTab } from "@/pages/rooms/RoomTimelineTab";
import { RoomAliasesTab } from "@/pages/rooms/RoomAliasesTab";
import { RoomHierarchyTab } from "@/pages/rooms/RoomHierarchyTab";
import { RoomMediaTab } from "@/pages/rooms/RoomMediaTab";
import { RoomExtremitiesTab } from "@/pages/rooms/RoomExtremitiesTab";
import { RoomActionDialog, type RoomAction } from "@/pages/rooms/RoomActionDialogs";

const TAB_LABELS: Record<RoomTab, string> = {
  overview: "Overview",
  state: "State",
  timeline: "Timeline",
  aliases: "Aliases",
  hierarchy: "Space",
  media: "Media",
  extremities: "Extremities",
};

/**
 * `/rooms/:id` — flows.md flow 3 steps 2-7: understand and act on a room. Readable with
 * `moderation:read`; the timeline and forward extremities need `admin:read` (decision 0013).
 */
export function RoomDetailPage() {
  const { roomId } = useParams({ from: "/rooms/$roomId" });
  const search = useSearch({ from: "/rooms/$roomId" });
  const navigate = useNavigate({ from: "/rooms/$roomId" });
  const { data: room, isLoading, isError, error, refetch } = useRoom(roomId);
  const hierarchy = useRoomHierarchy(roomId);
  const canModerate = hasScope("moderation:write");
  const canWrite = hasScope("admin:write");
  const [action, setAction] = useState<RoomAction | null>(null);

  const block = useBlockRoom();
  const unblock = useUnblockRoom();
  const makeAdmin = useMakeRoomAdmin();

  if (!hasScope("moderation:read")) {
    return (
      <div className="p-6">
        <ForbiddenState scope="moderation:read" />
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
  const isSpace =
    room.room_type === "m.space" ||
    (hierarchy.data?.items.some((n) => n.depth === 0 && n.children.length > 0) ?? false);
  const tabs = ROOM_TABS.filter((t) => t !== "hierarchy" || isSpace);
  const tab: RoomTab = search.tab && tabs.includes(search.tab) ? search.tab : "overview";
  const selectTab = (next: RoomTab) =>
    navigate({ search: { tab: next === "overview" ? undefined : next } });

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
            {room.room_type === "m.space" && (
              <Badge status="info" hideIcon>
                Space
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
            title={
              !canWrite
                ? "Needs admin:write"
                : "Joins you to the room with full power, granted by a local member who has it"
            }
            onClick={() =>
              makeAdmin.mutate(id, { onSuccess: () => toast({ title: "Joined as admin" }) })
            }
          >
            Make me admin
          </Button>
          <Button
            variant="secondary"
            disabled={!canWrite}
            title={!canWrite ? "Needs admin:write" : undefined}
            onClick={() => setAction("join")}
          >
            Join a user
          </Button>
          <Button
            variant="secondary"
            disabled={!canModerate}
            title={!canModerate ? "Needs moderation:write" : undefined}
            onClick={() => setAction("purge")}
          >
            Purge history
          </Button>
          {room.blocked ? (
            <Button
              variant="secondary"
              disabled={!canModerate}
              title={!canModerate ? "Needs moderation:write" : "Lets people join it again"}
              onClick={() =>
                unblock.mutate(id, { onSuccess: () => toast({ title: "Room unblocked" }) })
              }
            >
              Unblock
            </Button>
          ) : (
            <BlockButton
              room={room}
              disabled={!canModerate}
              onBlock={() =>
                block.mutate({ roomId: id }, { onSuccess: () => toast({ title: "Room blocked" }) })
              }
            />
          )}
          <Button
            variant="danger"
            disabled={!canModerate}
            title={!canModerate ? "Needs moderation:write" : undefined}
            onClick={() => setAction("delete")}
          >
            Delete room
          </Button>
        </div>
      </div>

      <div
        role="tablist"
        aria-label="Room sections"
        className="mt-6 flex flex-wrap gap-1 border-b border-border"
      >
        {tabs.map((t) => (
          <button
            key={t}
            type="button"
            role="tab"
            id={`room-tab-${t}`}
            aria-selected={tab === t}
            aria-controls="room-tab-panel"
            onClick={() => selectTab(t)}
            className={cn(
              "-mb-px border-b-2 px-3 py-2 text-sm",
              tab === t
                ? "border-accent font-medium text-text"
                : "border-transparent text-text-muted hover:text-text",
            )}
          >
            {TAB_LABELS[t]}
          </button>
        ))}
      </div>

      <div role="tabpanel" id="room-tab-panel" aria-labelledby={`room-tab-${tab}`} className="mt-6">
        {tab === "overview" && <RoomOverview room={room} />}
        {tab === "state" && <RoomStateTab roomId={id} />}
        {tab === "timeline" && (
          <RoomTimelineTab
            roomId={id}
            selected={search.event}
            onSelect={(event) => navigate({ search: { tab: "timeline", event } })}
          />
        )}
        {tab === "aliases" && <RoomAliasesTab roomId={id} />}
        {tab === "hierarchy" && <RoomHierarchyTab hierarchy={hierarchy} />}
        {tab === "media" && <RoomMediaTab roomId={id} />}
        {tab === "extremities" && <RoomExtremitiesTab roomId={id} />}
      </div>

      <RoomActionDialog action={action} room={room} onClose={() => setAction(null)} />
    </div>
  );
}

function BlockButton({
  room,
  disabled,
  onBlock,
}: {
  room: Room;
  disabled: boolean;
  onBlock: () => void;
}) {
  return (
    <Dialog>
      <DialogTrigger asChild>
        <Button variant="danger" disabled={disabled}>
          Block
        </Button>
      </DialogTrigger>
      <DialogContent
        title={`Block ${room.name ?? room.room_id}?`}
        description="Prevents new joins; current members remain and can still send messages."
        footer={
          <>
            <DialogClose asChild>
              <Button variant="secondary">Cancel</Button>
            </DialogClose>
            <DialogClose asChild>
              <Button variant="danger" onClick={onBlock}>
                Block
              </Button>
            </DialogClose>
          </>
        }
      />
    </Dialog>
  );
}

function RoomOverview({ room }: { room: Room }) {
  const {
    data: members,
    isError: membersIsError,
    error: membersError,
    refetch: refetchMembers,
  } = useRoomMembers(room.room_id);
  return (
    <div className="grid grid-cols-1 gap-8 xl:grid-cols-3">
      <div className="xl:col-span-2">
        <h2 className="text-md font-medium text-text">Overview</h2>
        <dl className="mt-3 grid grid-cols-1 gap-x-8 gap-y-4 sm:grid-cols-2">
          <Fact label="Creator" value={room.creator ?? "—"} />
          <Fact
            label="Room version"
            value={room.version ?? "—"}
            hint="The Matrix rules the room follows; an old version is upgraded by its admins."
          />
          <Fact label="Topic" value={room.topic ?? "—"} />
          <Fact label="Who can join" value={roomWords(JOIN_RULE_LABELS, room.join_rule)} />
          <Fact
            label="Who can read its history"
            value={roomWords(HISTORY_VISIBILITY_LABELS, room.history_visibility)}
          />
          <Fact
            label="Members (local / joined)"
            value={`${room.local_members_count ?? 0} / ${room.joined_members_count ?? 0}`}
          />
          <Fact
            label="State events"
            value={String(room.state_events_count ?? 0)}
            hint="Room settings and membership records; a very large number makes joining slow."
          />
          <Fact label="People on other servers can join" value={room.federatable ? "Yes" : "No"} />
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
                  {roomWords(MEMBERSHIP_LABELS, m.membership)}
                </Badge>
              </li>
            ))}
          </ul>
        )}
      </div>
    </div>
  );
}

function Fact({ label, value, hint }: { label: string; value: ReactNode; hint?: string }) {
  return (
    <div>
      <dt className="text-xs text-text-muted">{label}</dt>
      <dd className="mt-0.5 text-sm text-text">{value}</dd>
      {hint && <dd className="mt-0.5 text-xs text-text-faint">{hint}</dd>}
    </div>
  );
}
