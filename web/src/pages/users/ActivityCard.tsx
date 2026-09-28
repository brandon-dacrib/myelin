import { useState, type ReactNode } from "react";
import { Link } from "@tanstack/react-router";
import { Root, List, Trigger, Content } from "radix-ui/tabs";
import {
  useUserMedia,
  useUserMemberships,
  useUserSessions,
  useUserStatistics,
  type Membership,
  type MembershipState,
  type Session,
  type UserMediaItem,
} from "@/api/user-moderation";
import { Badge, type BadgeProps } from "@/components/ui/badge/Badge";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { Select } from "@/components/ui/select/Select";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { cn } from "@/lib/cn";
import { formatBytes, formatCount } from "@/lib/format";

const TABS = ["sessions", "rooms", "statistics", "media"] as const;
type Tab = (typeof TABS)[number];
const TAB_LABELS: Record<Tab, string> = {
  sessions: "Sessions",
  rooms: "Rooms",
  statistics: "Statistics",
  media: "Media",
};

/**
 * What a user has been doing: where they are signed in (support sessions marked), which rooms
 * they are in, what they have sent and uploaded. Each tab loads only when it is opened.
 */
export function ActivityCard({ userId }: { userId: string }) {
  const [tab, setTab] = useState<Tab>("sessions");
  return (
    <section aria-labelledby="activity-heading">
      <h2 id="activity-heading" className="text-md font-medium text-text">
        Activity
      </h2>
      <Root value={tab} onValueChange={(v) => setTab(v as Tab)} className="mt-2">
        <List aria-label="Activity" className="flex gap-1 border-b border-border">
          {TABS.map((t) => (
            <Trigger
              key={t}
              value={t}
              className={cn(
                "border-b-2 border-transparent px-3 py-2 text-sm font-medium text-text-muted",
                "data-[state=active]:border-accent data-[state=active]:text-accent",
                "hover:text-text focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-[var(--color-focus)]",
              )}
            >
              {TAB_LABELS[t]}
            </Trigger>
          ))}
        </List>
        <Content value="sessions" className="py-4">
          {tab === "sessions" && <SessionsTab userId={userId} />}
        </Content>
        <Content value="rooms" className="py-4">
          {tab === "rooms" && <RoomsTab userId={userId} />}
        </Content>
        <Content value="statistics" className="py-4">
          {tab === "statistics" && <StatisticsTab userId={userId} />}
        </Content>
        <Content value="media" className="py-4">
          {tab === "media" && <MediaTab userId={userId} />}
        </Content>
      </Root>
    </section>
  );
}

function SessionsTab({ userId }: { userId: string }) {
  const { data, isLoading, isError, error, refetch } = useUserSessions(userId);
  if (isError)
    return (
      <QueryProblemState
        error={error}
        resource="this user's sessions"
        onRetry={() => refetch()}
        compact
      />
    );
  const columns: Column<Session>[] = [
    {
      key: "device",
      header: "Session",
      priority: 1,
      render: (s) => (
        <div className="flex flex-col gap-1">
          <span className="font-identifier text-text">{s.device_id}</span>
          {s.display_name && <span className="text-xs text-text-muted">{s.display_name}</span>}
          {s.support_session && (
            <Badge status="warning" className="self-start">
              Support session
            </Badge>
          )}
        </div>
      ),
      renderCompact: (s) =>
        `${s.device_id}${s.support_session ? " (support session)" : ""}${
          s.display_name ? `, ${s.display_name}` : ""
        }`,
    },
    {
      key: "ip",
      header: "IP address",
      priority: 2,
      render: (s) =>
        s.ip ? <span className="font-identifier">{s.ip}</span> : <Faint>Not shown</Faint>,
    },
    {
      key: "user_agent",
      header: "Client",
      priority: 3,
      render: (s) =>
        s.user_agent ? (
          <span className="line-clamp-2 break-all text-xs">{s.user_agent}</span>
        ) : (
          <Faint>Unknown</Faint>
        ),
    },
    {
      key: "last_seen_at",
      header: "Last seen",
      priority: 1,
      render: (s) => <RelativeTime at={s.last_seen_at} />,
    },
  ];
  return (
    <DataTable
      caption="Sessions"
      columns={columns}
      rows={data?.items ?? []}
      getRowId={(s) => s.device_id ?? ""}
      loading={isLoading}
      density="compact"
      empty={<p className="p-4 text-sm text-text-muted">Not signed in anywhere.</p>}
    />
  );
}

const MEMBERSHIP_META: Record<
  MembershipState,
  { label: string; filter: string; status: NonNullable<BadgeProps["status"]> }
> = {
  join: { label: "Joined", filter: "Joined", status: "success" },
  invite: { label: "Invited", filter: "Invited", status: "info" },
  knock: { label: "Knocking", filter: "Knocking", status: "info" },
  leave: { label: "Left", filter: "Left", status: "neutral" },
  ban: { label: "Banned", filter: "Banned", status: "danger" },
};
const ALL = "all";

function RoomsTab({ userId }: { userId: string }) {
  const [filter, setFilter] = useState<string>(ALL);
  const membership = filter === ALL ? undefined : (filter as MembershipState);
  const { data, isLoading, isError, error, refetch } = useUserMemberships(userId, membership);
  const columns: Column<Membership>[] = [
    {
      key: "room",
      header: "Room",
      priority: 1,
      interactive: true,
      render: (m) =>
        m.room_id ? (
          <Link
            to="/rooms/$roomId"
            params={{ roomId: m.room_id }}
            className="flex flex-col text-text hover:text-accent"
          >
            <span className="font-medium hover:underline">{m.room_name ?? m.room_id}</span>
            {m.room_name && (
              <span className="font-identifier text-xs text-text-muted">{m.room_id}</span>
            )}
          </Link>
        ) : (
          <Faint>Unknown room</Faint>
        ),
    },
    {
      key: "membership",
      header: "Membership",
      priority: 1,
      render: (m) =>
        m.membership ? (
          <Badge status={MEMBERSHIP_META[m.membership].status} hideIcon>
            {MEMBERSHIP_META[m.membership].label}
          </Badge>
        ) : (
          <Faint>—</Faint>
        ),
      renderCompact: (m) => (m.membership ? MEMBERSHIP_META[m.membership].label : "—"),
    },
    {
      key: "display_name",
      header: "Name in room",
      priority: 2,
      render: (m) => m.display_name ?? <Faint>—</Faint>,
    },
  ];
  return (
    <div className="flex flex-col gap-3">
      <div className="w-48">
        <Select
          aria-label="Membership"
          value={filter}
          onValueChange={setFilter}
          options={[
            { value: ALL, label: "Every membership" },
            ...(Object.keys(MEMBERSHIP_META) as MembershipState[]).map((k) => ({
              value: k,
              label: MEMBERSHIP_META[k].filter,
            })),
          ]}
        />
      </div>
      {isError ? (
        <QueryProblemState
          error={error}
          resource="this user's rooms"
          onRetry={() => refetch()}
          compact
        />
      ) : (
        <DataTable
          caption="Rooms"
          columns={columns}
          rows={data?.items ?? []}
          getRowId={(m) => `${m.room_id}`}
          loading={isLoading}
          density="compact"
          empty={
            <p className="p-4 text-sm text-text-muted">
              {membership ? "No rooms with that membership." : "Not in any room."}
            </p>
          }
        />
      )}
    </div>
  );
}

function StatisticsTab({ userId }: { userId: string }) {
  const { data, isLoading, isError, error, refetch } = useUserStatistics(userId);
  if (isError)
    return (
      <QueryProblemState
        error={error}
        resource="this user's statistics"
        onRetry={() => refetch()}
        compact
      />
    );
  if (isLoading || !data) return <SkeletonText lines={3} />;
  return (
    <dl className="grid grid-cols-2 gap-x-8 gap-y-4 sm:grid-cols-4">
      <Stat label="Rooms joined" value={formatCount(data.joins_count)} />
      <Stat label="Rooms created" value={formatCount(data.rooms_created_count)} />
      <Stat label="Events sent" value={formatCount(data.events_sent_count)} />
      <Stat label="Invites sent" value={formatCount(data.invites_sent_count)} />
      <Stat label="Media files" value={formatCount(data.media_count)} />
      <Stat label="Media size" value={formatBytes(data.media_bytes)} />
      <Stat label="Sessions" value={formatCount(data.session_count)} />
    </dl>
  );
}

function MediaTab({ userId }: { userId: string }) {
  const { data, isLoading, isError, error, refetch } = useUserMedia(userId);
  if (isError)
    return (
      <QueryProblemState
        error={error}
        resource="this user's media"
        onRetry={() => refetch()}
        compact
      />
    );
  const columns: Column<UserMediaItem>[] = [
    {
      key: "name",
      header: "File",
      priority: 1,
      render: (m) => (
        <div className="flex flex-col">
          <span className="text-text">{m.upload_name ?? m.media_id}</span>
          <span className="font-identifier text-xs text-text-muted">
            mxc://{m.server_name}/{m.media_id}
          </span>
        </div>
      ),
      renderCompact: (m) => m.upload_name ?? m.media_id,
    },
    {
      key: "flags",
      header: "State",
      priority: 2,
      render: (m) => (
        <div className="flex flex-wrap gap-1">
          {m.protected && <Badge status="info">Protected</Badge>}
          {m.quarantined && <Badge status="danger">Quarantined</Badge>}
          {!m.protected && !m.quarantined && <Faint>—</Faint>}
        </div>
      ),
    },
    {
      key: "size",
      header: "Size",
      priority: 1,
      align: "end",
      render: (m) => <span className="tabular-nums">{formatBytes(m.size_bytes)}</span>,
    },
    {
      key: "created_at",
      header: "Uploaded",
      priority: 2,
      render: (m) => <RelativeTime at={m.created_at} />,
    },
  ];
  const total = (data as { total?: number } | undefined)?.total;
  return (
    <div className="flex flex-col gap-2">
      {total != null && (
        <p className="text-sm text-text-muted">
          {total.toLocaleString()} {total === 1 ? "file" : "files"}
        </p>
      )}
      <DataTable
        caption="Media"
        columns={columns}
        rows={data?.items ?? []}
        getRowId={(m) => `${m.server_name}/${m.media_id}`}
        loading={isLoading}
        density="compact"
        empty={<p className="p-4 text-sm text-text-muted">Nothing uploaded.</p>}
      />
    </div>
  );
}

function Stat({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div>
      <dt className="text-xs text-text-muted">{label}</dt>
      <dd className="mt-0.5 text-lg tabular-nums text-text">{value}</dd>
    </div>
  );
}

function Faint({ children }: { children: ReactNode }) {
  return <span className="text-text-faint">{children}</span>;
}
