import { useMemo, useState } from "react";
import { useRoomState, type StateEvent } from "@/api/room-contents";
import { Input } from "@/components/ui/input/Input";
import { SkeletonText } from "@/components/ui/skeleton/Skeleton";
import { QueryProblemState } from "@/components/QueryProblemState";

/** The room's current state: one row per `(type, state_key)`, each with its content on demand. */
export function RoomStateTab({ roomId }: { roomId: string }) {
  const [filter, setFilter] = useState("");
  const state = useRoomState(roomId);
  const rows = useMemo(() => {
    const items = state.data?.items ?? [];
    const q = filter.trim().toLowerCase();
    return q ? items.filter((e) => e.type.toLowerCase().includes(q)) : items;
  }, [state.data, filter]);

  if (state.isLoading) return <SkeletonText lines={4} />;
  if (state.isError) {
    return (
      <QueryProblemState
        error={state.error}
        resource="this room's state"
        onRetry={() => state.refetch()}
      />
    );
  }
  return (
    <section aria-labelledby="room-state-heading">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2 id="room-state-heading" className="text-md font-medium text-text">
          State
        </h2>
        <Input
          className="max-w-xs"
          aria-label="Filter state by event type"
          placeholder="Filter by type, e.g. m.room.member"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
      </div>
      <p className="mt-1 text-sm text-text-muted">
        {rows.length.toLocaleString()} of {(state.data?.items.length ?? 0).toLocaleString()} state
        events.
      </p>
      <table className="mt-3 w-full text-left text-sm" aria-label="Room state">
        <thead className="text-xs text-text-muted">
          <tr>
            <th className="py-2 pr-3 font-medium">Type</th>
            <th className="py-2 pr-3 font-medium">State key</th>
            <th className="py-2 pr-3 font-medium">Sender</th>
            <th className="py-2 font-medium">Content</th>
          </tr>
        </thead>
        <tbody className="divide-y divide-border">
          {rows.map((event) => (
            <StateRow key={event.event_id} event={event} />
          ))}
        </tbody>
      </table>
    </section>
  );
}

function StateRow({ event }: { event: StateEvent }) {
  return (
    <tr className="align-top">
      <td className="py-2 pr-3 font-identifier text-text">{event.type}</td>
      <td className="py-2 pr-3 font-identifier text-text-muted">{event.state_key || "—"}</td>
      <td className="py-2 pr-3 font-identifier text-text-muted">{event.sender}</td>
      <td className="py-2">
        <details>
          <summary className="cursor-pointer text-xs text-accent">Show content</summary>
          <pre className="mt-1 max-w-xl overflow-x-auto rounded-sm bg-surface-sunken p-2 text-xs text-text">
            {JSON.stringify(event.content, null, 2)}
          </pre>
        </details>
      </td>
    </tr>
  );
}
