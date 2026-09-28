import { useState } from "react";
import { Eye } from "lucide-react";
import { useEventAt, useEventContext, useRoomTimeline, type RoomEvent } from "@/api/room-contents";
import { Button } from "@/components/ui/button/Button";
import { Badge } from "@/components/ui/badge/Badge";
import { Field, Input } from "@/components/ui/input/Input";
import { ForbiddenState } from "@/components/ui/error-state/ErrorState";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { QueryProblemState } from "@/components/QueryProblemState";
import { MutationError } from "@/components/MutationError";
import { hasScope } from "@/lib/auth";
import { summarizeEvent } from "@/lib/rooms";
import { cn } from "@/lib/cn";

function when(ts: number): string {
  return new Date(ts).toLocaleString();
}

/**
 * The room's timeline, newest first, as an administrator reads it: `admin:read` only, and every
 * read is on the audit log (decision 0013). Clicking an event shows it with what surrounds it;
 * "Jump to date" finds the event nearest a moment (`rooms.events.at`).
 */
export function RoomTimelineTab({
  roomId,
  selected,
  onSelect,
}: {
  roomId: string;
  selected: string | undefined;
  onSelect: (eventId: string | undefined) => void;
}) {
  const canRead = hasScope("admin:read");
  const timeline = useRoomTimeline(roomId, canRead);
  const eventAt = useEventAt();
  const [jumpTo, setJumpTo] = useState("");

  if (!canRead) return <ForbiddenState scope="admin:read" compact />;

  const events = timeline.data?.pages.flatMap((p) => p.items) ?? [];
  return (
    <section aria-labelledby="room-timeline-heading">
      <h2 id="room-timeline-heading" className="text-md font-medium text-text">
        Timeline
      </h2>
      <p className="mt-1 flex items-start gap-2 rounded-sm border border-info-border bg-info-bg p-2 text-sm text-text">
        <Eye size={16} aria-hidden="true" className="mt-0.5 shrink-0 text-info" />
        Reading a room&apos;s messages is recorded in the audit log, under your name.
      </p>

      <form
        className="mt-3 flex flex-wrap items-end gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          const ts = Date.parse(jumpTo);
          if (Number.isNaN(ts)) return;
          eventAt.mutate(
            { roomId, ts, dir: "f" },
            { onSuccess: (event) => onSelect(event.event_id) },
          );
        }}
      >
        <Field label="Jump to date">
          {(field) => (
            <Input
              {...field}
              type="datetime-local"
              step={1}
              value={jumpTo}
              onChange={(e) => setJumpTo(e.target.value)}
              className="w-56"
            />
          )}
        </Field>
        <Button type="submit" variant="secondary" disabled={!jumpTo || eventAt.isPending}>
          Jump
        </Button>
      </form>
      {eventAt.isError && (
        <MutationError className="mt-2" error={eventAt.error} action="find an event at that time" />
      )}

      <div className="mt-4 grid grid-cols-1 gap-6 xl:grid-cols-2">
        <div>
          {timeline.isLoading ? (
            <SkeletonText lines={6} />
          ) : timeline.isError ? (
            <QueryProblemState
              error={timeline.error}
              resource="this room's timeline"
              scope="admin:read"
              onRetry={() => timeline.refetch()}
            />
          ) : events.length === 0 ? (
            <p className="text-sm text-text-muted">No messages.</p>
          ) : (
            <>
              <ol
                aria-label="Messages"
                className="divide-y divide-border rounded-md border border-border"
              >
                {events.map((event) => (
                  <li key={event.event_id}>
                    <EventRow
                      event={event}
                      active={event.event_id === selected}
                      onClick={() => onSelect(event.event_id)}
                    />
                  </li>
                ))}
              </ol>
              {timeline.hasNextPage && (
                <Button
                  className="mt-3"
                  variant="secondary"
                  disabled={timeline.isFetchingNextPage}
                  onClick={() => timeline.fetchNextPage()}
                >
                  Load older
                </Button>
              )}
            </>
          )}
        </div>
        {selected && <EventContextPanel roomId={roomId} eventId={selected} onSelect={onSelect} />}
      </div>
    </section>
  );
}

function EventRow({
  event,
  active,
  onClick,
}: {
  event: RoomEvent;
  active?: boolean;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      className={cn(
        "flex w-full flex-col gap-0.5 px-3 py-2 text-left hover:bg-surface-sunken",
        active && "bg-surface-sunken",
      )}
    >
      <span className="flex flex-wrap items-center gap-2 text-xs text-text-muted">
        <span className="font-identifier">{event.sender}</span>
        <span>{when(event.origin_server_ts)}</span>
        <span className="font-identifier">{event.type}</span>
        {event.redacted && (
          <Badge status="muted" hideIcon>
            Redacted
          </Badge>
        )}
      </span>
      <span className="text-sm text-text">{summarizeEvent(event) || "—"}</span>
    </button>
  );
}

function EventContextPanel({
  roomId,
  eventId,
  onSelect,
}: {
  roomId: string;
  eventId: string;
  onSelect: (eventId: string | undefined) => void;
}) {
  const context = useEventContext(roomId, eventId, 3);
  return (
    <aside aria-label="Event in context" className="rounded-md border border-border p-3">
      <div className="flex items-center justify-between gap-2">
        <h3 className="text-sm font-medium text-text">Event in context</h3>
        <Button size="sm" variant="ghost" onClick={() => onSelect(undefined)}>
          Close
        </Button>
      </div>
      {context.isLoading ? (
        <SkeletonText lines={4} />
      ) : context.isError ? (
        <QueryProblemState error={context.error} resource="this event" compact />
      ) : context.data ? (
        <div className="mt-2 flex flex-col gap-2">
          <ol aria-label="Before" className="flex flex-col-reverse gap-1">
            {context.data.events_before.map((e) => (
              <li key={e.event_id} className="text-xs text-text-muted">
                {summarizeEvent(e) || e.type}
              </li>
            ))}
          </ol>
          <div className="rounded-sm border border-accent p-2">
            <p className="font-identifier text-xs text-text-muted">{context.data.event.event_id}</p>
            <p className="text-sm text-text">
              {summarizeEvent(context.data.event) || context.data.event.type}
            </p>
            <pre className="mt-1 max-h-60 overflow-auto rounded-sm bg-surface-sunken p-2 text-xs text-text">
              {JSON.stringify(context.data.event.content, null, 2)}
            </pre>
          </div>
          <ol aria-label="After" className="flex flex-col gap-1">
            {context.data.events_after.map((e) => (
              <li key={e.event_id} className="text-xs text-text-muted">
                {summarizeEvent(e) || e.type}
              </li>
            ))}
          </ol>
          <p className="text-xs text-text-muted">
            {context.data.state.length.toLocaleString()} state events at this point.
          </p>
        </div>
      ) : null}
    </aside>
  );
}
