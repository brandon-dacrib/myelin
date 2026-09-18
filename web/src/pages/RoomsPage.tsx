import { useState } from "react";
import { useNavigate, useSearch, Link } from "@tanstack/react-router";
import { DoorOpen } from "lucide-react";
import { useRooms, type Room } from "@/api/rooms";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { Input } from "@/components/ui/input/Input";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ErrorState, ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { hasScope } from "@/lib/auth";

/** `/rooms` — flows.md flow 3: understand a room. */
export function RoomsPage() {
  const search = useSearch({ from: "/rooms" });
  const navigate = useNavigate({ from: "/rooms" });
  const [queryInput, setQueryInput] = useState(search.q ?? "");
  const canRead = hasScope("admin:read");

  const { data, isLoading, isError, refetch } = useRooms({
    q: search.q,
    cursor: search.cursor,
    limit: 20,
  });

  const columns: Column<Room>[] = [
    {
      key: "room_id",
      header: "Room",
      priority: 1,
      interactive: true,
      render: (r) => (
        <Link
          to="/rooms/$roomId"
          params={{ roomId: r.room_id }}
          className="font-medium text-text hover:text-accent hover:underline"
        >
          {r.name ?? r.canonical_alias ?? r.room_id}
        </Link>
      ),
      renderCompact: (r) => r.name ?? r.canonical_alias ?? r.room_id,
    },
    {
      key: "flags",
      header: "Flags",
      priority: 1,
      render: (r) => (
        <div className="flex flex-wrap gap-1">
          {r.public && (
            <Badge status="info" hideIcon>
              Public
            </Badge>
          )}
          {r.encrypted && (
            <Badge status="muted" hideIcon>
              Encrypted
            </Badge>
          )}
          {r.blocked && <Badge status="danger">Blocked</Badge>}
        </div>
      ),
      renderCompact: (r) =>
        [r.public && "Public", r.encrypted && "Encrypted", r.blocked && "Blocked"]
          .filter(Boolean)
          .join(", ") || "—",
    },
    {
      key: "members",
      header: "Members",
      priority: 2,
      align: "end",
      render: (r) => `${r.local_members_count ?? 0} / ${r.joined_members_count ?? 0}`,
    },
    { key: "version", header: "Version", priority: 3, render: (r) => r.version ?? "—" },
  ];

  if (!canRead) {
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Rooms</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <h1 className="text-xl text-text">Rooms</h1>

      <form
        className="mt-4 flex max-w-md gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          navigate({ search: { q: queryInput || undefined } });
        }}
      >
        <Input
          aria-label="Search by room ID, alias or name"
          placeholder="Search by ID, alias or name..."
          value={queryInput}
          onChange={(e) => setQueryInput(e.target.value)}
        />
        <Button type="submit">Search</Button>
        {search.q && (
          <Button
            type="button"
            variant="ghost"
            onClick={() => {
              setQueryInput("");
              navigate({ search: {} });
            }}
          >
            Clear
          </Button>
        )}
      </form>

      {isError && (
        <div className="mt-6">
          <ErrorState title="Couldn't load rooms" onRetry={() => refetch()} />
        </div>
      )}

      {!isError && (
        <div className="mt-4">
          <DataTable
            caption="Rooms"
            columns={columns}
            rows={data?.items ?? []}
            getRowId={(r) => r.room_id}
            loading={isLoading}
            onRowClick={(r) => navigate({ to: "/rooms/$roomId", params: { roomId: r.room_id } })}
            empty={
              search.q ? (
                <EmptyState
                  variant="filtered"
                  icon={<DoorOpen aria-hidden="true" />}
                  title="No rooms match"
                  description={`No results for "${search.q}".`}
                />
              ) : (
                <EmptyState
                  icon={<DoorOpen aria-hidden="true" />}
                  title="No rooms yet"
                  description="Rooms appear when users create or join them."
                />
              )
            }
            pagination={{
              hasPrevious: false,
              hasNext: Boolean(data?.next_cursor),
              onPrevious: () => navigate({ search: { ...search, cursor: undefined } }),
              onNext: () =>
                data?.next_cursor &&
                navigate({ search: { ...search, cursor: data.next_cursor ?? undefined } }),
            }}
          />
        </div>
      )}
    </div>
  );
}
