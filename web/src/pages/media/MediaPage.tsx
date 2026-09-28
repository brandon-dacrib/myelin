import { useCallback, useState } from "react";
import { useNavigate, useSearch } from "@tanstack/react-router";
import { Image } from "lucide-react";
import {
  mediaKey,
  mediaName,
  useMediaList,
  type MediaItem,
  type MediaListFilters,
  type MediaSort,
  type Task,
} from "@/api/media";
import type { MediaSearch } from "./media-search";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Input } from "@/components/ui/input/Input";
import { Select } from "@/components/ui/select/Select";
import { DataTable, type Column } from "@/components/ui/table/DataTable";
import { EmptyState } from "@/components/ui/empty-state/EmptyState";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { hasScope } from "@/lib/auth";
import { formatBytes, formatCount } from "@/lib/format";
import { BulkDeleteDialog, PurgeRemoteCacheDialog } from "./BulkMediaDialogs";
import { BulkTaskBanner } from "./BulkTaskBanner";
import { MediaDetailSheet } from "./MediaDetailSheet";
import { MediaThumbnail } from "./MediaThumbnail";

const ORIGIN_OPTIONS = [
  { value: "all", label: "Local and remote" },
  { value: "local", label: "Uploaded here" },
  { value: "remote", label: "Cached from other servers" },
];
const STATUS_OPTIONS = [
  { value: "all", label: "Any status" },
  { value: "quarantined", label: "Quarantined" },
  { value: "protected", label: "Protected" },
];
const SORT_OPTIONS: { value: MediaSort; label: string }[] = [
  { value: "-created_at", label: "Newest first" },
  { value: "created_at", label: "Oldest first" },
  { value: "-size_bytes", label: "Largest first" },
  { value: "-last_accessed_at", label: "Recently viewed" },
  { value: "last_accessed_at", label: "Least recently viewed" },
];

/** `/media` — every upload and cached remote copy; find it, preview it, act on it. */
export function MediaPage() {
  const search = useSearch({ from: "/media" });
  const navigate = useNavigate({ from: "/media" });
  const [queryInput, setQueryInput] = useState(search.q ?? "");
  const [selected, setSelected] = useState<MediaItem | null>(null);
  // Bulk deletions started from this page, followed until they end.
  const [followed, setFollowed] = useState<string[]>([]);
  const follow = useCallback((task: Task) => setFollowed((ids) => [...ids, task.id]), []);
  const unfollow = useCallback(
    (id: string) => setFollowed((ids) => ids.filter((i) => i !== id)),
    [],
  );
  const canRead = hasScope("admin:read");

  const filters: MediaListFilters = {
    q: search.q,
    origin: search.origin,
    quarantined: search.status === "quarantined" ? true : undefined,
    protected: search.status === "protected" ? true : undefined,
    sort: search.sort,
    cursor: search.cursor,
    limit: 25,
  };
  const { data, isLoading, isError, error, refetch } = useMediaList(filters);
  const filtered = Boolean(search.q || search.origin || search.status);

  function setFilter(next: Partial<MediaSearch>) {
    navigate({ search: { ...search, ...next, cursor: undefined } });
  }

  const columns: Column<MediaItem>[] = [
    {
      key: "preview",
      header: "Preview",
      priority: 2,
      render: (m) => <MediaThumbnail item={m} size="row" />,
      renderCompact: () => null,
    },
    {
      key: "name",
      header: "File",
      priority: 1,
      interactive: true,
      render: (m) => (
        <div className="min-w-0">
          <button
            type="button"
            onClick={() => setSelected(m)}
            className="max-w-[28ch] truncate text-left font-medium text-text hover:text-accent hover:underline"
          >
            {mediaName(m)}
          </button>
          <div className="truncate font-identifier text-xs text-text-muted">{mediaKey(m)}</div>
        </div>
      ),
      renderCompact: (m) => mediaName(m),
    },
    {
      key: "status",
      header: "Status",
      priority: 1,
      render: (m) => (
        <div className="flex flex-wrap gap-1">
          {m.quarantined && <Badge status="danger">Quarantined</Badge>}
          {m.protected && <Badge status="success">Protected</Badge>}
          {m.origin === "remote" && (
            <Badge status="neutral" hideIcon>
              Remote
            </Badge>
          )}
        </div>
      ),
      renderCompact: (m) =>
        [
          m.quarantined && "Quarantined",
          m.protected && "Protected",
          m.origin === "remote" && "Remote",
        ]
          .filter(Boolean)
          .join(", ") || "—",
    },
    {
      key: "uploader",
      header: "Uploader",
      priority: 2,
      render: (m) =>
        m.uploader ? (
          <span className="font-identifier">{m.uploader}</span>
        ) : (
          <span className="text-text-muted">{m.server_name}</span>
        ),
    },
    {
      key: "type",
      header: "Type",
      priority: 3,
      render: (m) => m.content_type ?? "—",
    },
    {
      key: "size",
      header: "Size",
      priority: 1,
      align: "end",
      render: (m) => <span className="tabular-nums">{formatBytes(m.size_bytes)}</span>,
      renderCompact: (m) => formatBytes(m.size_bytes),
    },
    {
      key: "created_at",
      header: "Uploaded",
      priority: 2,
      render: (m) => <RelativeTime at={m.created_at} />,
    },
    {
      key: "last_accessed_at",
      header: "Last viewed",
      priority: 3,
      render: (m) => <RelativeTime at={m.last_accessed_at} />,
    },
  ];

  if (!canRead) {
    return (
      <div className="p-6">
        <h1 className="text-xl text-text">Media</h1>
        <ForbiddenState scope="admin:read" />
      </div>
    );
  }

  return (
    <div className="mx-auto max-w-[90rem] p-6">
      <div className="flex flex-wrap items-start justify-between gap-4">
        <div>
          <h1 className="text-xl text-text">Media</h1>
          <p className="mt-1 text-sm text-text-muted">
            Files uploaded here, and this server&apos;s copies of files from other servers.
            {data?.total != null && ` ${formatCount(data.total)} ${filtered ? "match" : "in all"}.`}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <BulkDeleteDialog disabled={!hasScope("moderation:write")} onStarted={follow} />
          <PurgeRemoteCacheDialog disabled={!hasScope("admin:write")} onStarted={follow} />
        </div>
      </div>

      <BulkTaskBanner followed={followed} onEnded={unfollow} />

      <div className="mt-4 flex flex-wrap items-end gap-2">
        <form
          className="flex min-w-[16rem] flex-1 gap-2 sm:max-w-md"
          onSubmit={(e) => {
            e.preventDefault();
            setFilter({ q: queryInput.trim() || undefined });
          }}
        >
          <Input
            aria-label="Search by file name, uploader, type or ID"
            placeholder="Search by name, uploader, type or ID..."
            value={queryInput}
            onChange={(e) => setQueryInput(e.target.value)}
          />
          <Button type="submit">Search</Button>
        </form>
        <div className="w-52">
          <Select
            aria-label="Origin"
            value={search.origin ?? "all"}
            options={ORIGIN_OPTIONS}
            onValueChange={(v) =>
              setFilter({ origin: v === "all" ? undefined : (v as "local" | "remote") })
            }
          />
        </div>
        <div className="w-40">
          <Select
            aria-label="Status"
            value={search.status ?? "all"}
            options={STATUS_OPTIONS}
            onValueChange={(v) =>
              setFilter({
                status: v === "all" ? undefined : (v as "quarantined" | "protected"),
              })
            }
          />
        </div>
        <div className="w-52">
          <Select
            aria-label="Sort"
            value={search.sort ?? "-created_at"}
            options={SORT_OPTIONS}
            onValueChange={(v) => setFilter({ sort: v as MediaSort })}
          />
        </div>
        {filtered && (
          <Button
            type="button"
            variant="ghost"
            onClick={() => {
              setQueryInput("");
              navigate({ search: { sort: search.sort } });
            }}
          >
            Clear filters
          </Button>
        )}
      </div>

      {isError && (
        <div className="mt-6">
          <QueryProblemState
            error={error}
            resource="media"
            scope="admin:read"
            onRetry={() => refetch()}
          />
        </div>
      )}

      {!isError && (
        <div className="mt-4">
          <DataTable
            caption="Media"
            columns={columns}
            rows={data?.items ?? []}
            getRowId={(m) => mediaKey(m)}
            loading={isLoading}
            onRowClick={(m) => setSelected(m)}
            empty={
              filtered ? (
                <EmptyState
                  variant="filtered"
                  icon={<Image aria-hidden="true" />}
                  title="No media matches"
                  description={
                    search.q ? `No results for "${search.q}".` : "Nothing matches these filters."
                  }
                />
              ) : (
                <EmptyState
                  icon={<Image aria-hidden="true" />}
                  title="No media yet"
                  description="Files appear here when people upload them, or view files from other servers."
                />
              )
            }
            pagination={{
              hasPrevious: Boolean(data?.prev_cursor),
              hasNext: Boolean(data?.next_cursor),
              onPrevious: () =>
                navigate({ search: { ...search, cursor: data?.prev_cursor ?? undefined } }),
              onNext: () =>
                data?.next_cursor &&
                navigate({ search: { ...search, cursor: data.next_cursor ?? undefined } }),
            }}
          />
        </div>
      )}

      <MediaDetailSheet selected={selected} onClose={() => setSelected(null)} />
    </div>
  );
}
