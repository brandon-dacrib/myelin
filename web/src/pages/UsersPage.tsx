import { useState } from "react";
import { useNavigate, useSearch, Link } from "@tanstack/react-router";
import { Link2, UserPlus, Users as UsersIcon } from "lucide-react";
import { useUsers, type User } from "@/api/users";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { Input } from "@/components/ui/input/Input";
import { Switch } from "@/components/ui/switch/Switch";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { hasScope } from "@/lib/auth";
import { AddUserDialog } from "./users/AddUserDialog";
import { CreateTokenDialog } from "./settings/CreateTokenDialog";

/** `/users` — flows.md flow 2: find and deal with a user. */
export function UsersPage() {
  const search = useSearch({ from: "/users" });
  const navigate = useNavigate({ from: "/users" });
  const [queryInput, setQueryInput] = useState(search.q ?? "");
  const canRead = hasScope("admin:read");
  const canWrite = hasScope("admin:write");
  const [addOpen, setAddOpen] = useState(false);
  // An invite link lets the person choose their own username and password, where Add user has
  // the administrator choose them and hand them over.
  const [inviteOpen, setInviteOpen] = useState(false);

  const { data, isLoading, isError, error, refetch } = useUsers({
    q: search.q,
    cursor: search.cursor,
    suspended: search.suspended,
    limit: 20,
  });

  const columns: Column<User>[] = [
    {
      key: "user_id",
      header: "User",
      priority: 1,
      interactive: true,
      render: (u) => (
        <Link
          to="/users/$userId"
          params={{ userId: u.user_id }}
          className="font-identifier font-medium text-text hover:text-accent hover:underline"
        >
          {u.user_id}
        </Link>
      ),
    },
    {
      key: "display_name",
      header: "Display name",
      priority: 2,
      render: (u) => u.display_name ?? "—",
    },
    {
      key: "status",
      header: "Status",
      priority: 1,
      render: (u) => (
        <div className="flex flex-wrap gap-1">
          {u.admin && (
            <Badge status="info" hideIcon>
              Admin
            </Badge>
          )}
          {u.locked && <Badge status="warning">Locked</Badge>}
          {u.suspended && <Badge status="warning">Suspended</Badge>}
          {u.deactivated && <Badge status="danger">Deactivated</Badge>}
          {u.shadow_banned && (
            <Badge status="muted" hideIcon>
              Shadow-banned
            </Badge>
          )}
          {u.is_guest && (
            <span title="A guest account: no password, and it can only read and talk in rooms that let guests in. Guests exist while Configuration, Authentication, allow guest access is on.">
              <Badge status="neutral">Guest</Badge>
            </span>
          )}
          {!u.admin && !u.locked && !u.suspended && !u.deactivated && !u.shadow_banned && (
            <Badge status="success">Active</Badge>
          )}
        </div>
      ),
      renderCompact: (u) =>
        [
          u.admin && "Admin",
          u.locked && "Locked",
          u.suspended && "Suspended",
          u.deactivated && "Deactivated",
          u.shadow_banned && "Shadow-banned",
          u.is_guest && "Guest",
        ]
          .filter(Boolean)
          .join(", ") || "Active",
    },
    {
      key: "last_seen_at",
      header: "Last seen",
      priority: 2,
      render: (u) => <RelativeTime at={u.last_seen_at} />,
    },
    {
      key: "device_count",
      header: "Devices",
      priority: 3,
      align: "end",
      render: (u) => u.device_count ?? 0,
    },
  ];

  if (!canRead) {
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Users</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h1 className="text-xl text-text">Users</h1>
        {canWrite && (
          <div className="flex flex-wrap gap-2">
            <Button
              variant="secondary"
              leadingIcon={<Link2 size={16} aria-hidden="true" />}
              onClick={() => setInviteOpen(true)}
            >
              Invite by link
            </Button>
            <Button
              leadingIcon={<UserPlus size={16} aria-hidden="true" />}
              onClick={() => setAddOpen(true)}
            >
              Add user
            </Button>
          </div>
        )}
      </div>
      <AddUserDialog open={addOpen} onOpenChange={setAddOpen} />
      <CreateTokenDialog open={inviteOpen} onOpenChange={setInviteOpen} />

      <form
        className="mt-4 flex max-w-md gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          navigate({ search: { q: queryInput || undefined, suspended: search.suspended } });
        }}
      >
        <Input
          aria-label="Search by Matrix ID, display name, email or external ID"
          placeholder="Search by ID, display name, email..."
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

      <div className="mt-3 flex items-center gap-2">
        <Switch
          id="users-suspended-only"
          checked={Boolean(search.suspended)}
          onCheckedChange={(on) =>
            navigate({ search: { q: search.q, suspended: on ? true : undefined } })
          }
        />
        <label htmlFor="users-suspended-only" className="text-sm text-text">
          Suspended only
        </label>
      </div>

      {isError && (
        <div className="mt-6">
          <QueryProblemState
            error={error}
            resource="users"
            scope="admin:read"
            onRetry={() => refetch()}
          />
        </div>
      )}

      {!isError && (
        <div className="mt-4">
          <DataTable
            caption="Users"
            columns={columns}
            rows={data?.items ?? []}
            getRowId={(u) => u.user_id}
            loading={isLoading}
            onRowClick={(u) => navigate({ to: "/users/$userId", params: { userId: u.user_id } })}
            empty={
              search.q || search.suspended ? (
                <EmptyState
                  variant="filtered"
                  icon={<UsersIcon aria-hidden="true" />}
                  title="No user matches"
                  description={search.q ? `No results for "${search.q}".` : "Nobody is suspended."}
                  action={
                    <Button
                      variant="ghost"
                      onClick={() => {
                        setQueryInput("");
                        navigate({ search: {} });
                      }}
                    >
                      Clear search
                    </Button>
                  }
                />
              ) : (
                <EmptyState
                  icon={<UsersIcon aria-hidden="true" />}
                  title="You are the only user so far"
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
