import {
  useDiscardAppserviceDeadLetters,
  useReplayAppserviceBacklog,
  type AppServiceBacklogEntry,
  type AppServiceQueue,
} from "@/api/bridges";
import { ApiProblemError } from "@/api/problem";
import { Badge } from "@/components/ui/badge/Badge";
import { Button } from "@/components/ui/button/Button";
import { Dialog, DialogClose, DialogContent, DialogTrigger } from "@/components/ui/dialog/Dialog";
import { QueryProblemState } from "@/components/QueryProblemState";
import { RelativeTime } from "@/components/RelativeTime";
import { toast } from "@/components/ui/toast/toast-store";
import {
  describeBacklogContents,
  formatBacklogEntry,
  oldestDeadLetterAgeMs,
  shortRoomId,
} from "@/lib/bridge-state";
import { formatDuration } from "@/lib/format";

/** The room ids a backlog entry lists at most (`hs_appservice::registry::BACKLOG_ROOMS`). */
const BACKLOG_ROOMS = 5;

function plural(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** What the server said when it refused, for a toast's second line. */
function problemDetail(error: unknown): string | undefined {
  if (error instanceof ApiProblemError) return error.problem.detail ?? error.problem.title;
  return undefined;
}

export interface BridgeTransactionsProps {
  /** The appservice id. */
  id: string;
  /** The bridge's display name, for the dialogs and toasts. */
  name: string;
  canWrite: boolean;
  /** The first page of `GET /appservices/{id}/backlog`, oldest first. */
  entries: AppServiceBacklogEntry[] | undefined;
  /** `AppService.queue`: the whole queue's counts, which the page above may not hold all of. */
  queue: AppServiceQueue | undefined;
  isError: boolean;
  error: unknown;
}

/**
 * The bridge page's Transactions tab: what waits to be sent to the bridge, entry by entry (when
 * it was queued, what it carries and in which rooms, how often it was tried and the last error),
 * and what to do about dead letters. Replay sends them again now; discard deletes them for good
 * (OpenAPI 0.1.18). Both confirm first, saying how many and how old, because replaying a
 * days-old incident sends the bridge days-old events: on 2026-10-15 a "Replay all" on the demo's
 * Discord bridge re-sent about 7,000 three-day-old events and swamped it.
 */
export function BridgeTransactions({
  id,
  name,
  canWrite,
  entries,
  queue,
  isError,
  error,
}: BridgeTransactionsProps) {
  const replay = useReplayAppserviceBacklog();
  const discard = useDiscardAppserviceDeadLetters();

  if (isError) return <QueryProblemState error={error} resource="transactions" />;

  const items = entries ?? [];
  const loadedDead = items.filter((e) => e.dead_lettered).length;
  // The whole queue's count when the server gives it: the backlog page holds at most 50.
  const deadCount = Math.max(queue?.dead_lettered ?? 0, loadedDead);
  const totalCount = Math.max((queue?.pending ?? 0) + (queue?.dead_lettered ?? 0), items.length);
  const oldestAge = oldestDeadLetterAgeMs(items);
  const oldestText = oldestAge === undefined ? undefined : formatDuration(oldestAge);
  const deadNoun = plural(deadCount, "dead-lettered transaction");
  const writeTitle = !canWrite ? "Needs bridges:write" : undefined;

  return (
    <div className="flex flex-col gap-4">
      <p className="max-w-3xl text-sm text-text-muted">
        What the server has queued for {name} and not yet delivered, oldest first. Each entry is one
        room&apos;s page of events or one batch of ephemeral data (typing, receipts, presence); up
        to twenty, and at most 100 events, are sent together as one transaction (fewer while the
        bridge is slow to answer). Waiting entries are retried on their own, under the same
        transaction id.
      </p>

      {deadCount > 0 && (
        <div
          className="flex flex-col gap-3 rounded-md border border-warning-border bg-warning-bg px-4 py-3"
          data-testid="dead-letters"
        >
          <p className="text-sm font-medium text-warning">
            {deadNoun}
            {oldestText ? `, the oldest queued ${oldestText} ago` : ""}.
          </p>
          <p className="max-w-3xl text-sm text-text">
            The server gave up on these: their retry budget ran out because the bridge refused each
            of them 10 times, or for a whole day could not be reached, did not answer in time, or
            answered 5xx, 408 or 429. They are kept until you replay them (send them again now, with
            a fresh retry budget) or discard them (delete them for good). Replaying sends the bridge
            those events now, however old they are; for entries from an incident, such as days-old
            events or events in rooms the bridge does not bridge, discarding is usually right.
          </p>
          {totalCount > items.length && (
            <p className="text-sm text-text-muted">
              The first {items.length} of {totalCount} entries are listed below.
            </p>
          )}
          <div className="flex flex-wrap gap-2">
            <Dialog>
              <DialogTrigger asChild>
                <Button variant="secondary" size="sm" disabled={!canWrite} title={writeTitle}>
                  Replay all
                </Button>
              </DialogTrigger>
              <DialogContent
                title={`Replay ${deadNoun}?`}
                description={
                  <>
                    The server sends {deadCount === 1 ? "it" : "them"} to {name} again now, each
                    with a fresh retry budget.
                    {oldestText
                      ? ` The oldest was queued ${oldestText} ago: the bridge receives events that old now, as if they were new.`
                      : " The bridge receives their events now, however old they are."}{" "}
                    If they are from an incident (days-old events, or events in rooms the bridge
                    does not bridge), discard them instead.
                  </>
                }
                footer={
                  <>
                    <DialogClose asChild>
                      <Button variant="secondary">Cancel</Button>
                    </DialogClose>
                    <DialogClose asChild>
                      <Button
                        onClick={() =>
                          replay.mutate(id, {
                            onSuccess: (task) =>
                              toast({
                                title: `Replaying ${deadNoun} to ${name}`,
                                description: task?.id ? `Task ${task.id}` : undefined,
                              }),
                            onError: (err) =>
                              toast({
                                title: "Couldn't start the replay",
                                description: problemDetail(err),
                                variant: "danger",
                              }),
                          })
                        }
                      >
                        Replay all
                      </Button>
                    </DialogClose>
                  </>
                }
              />
            </Dialog>
            <Dialog>
              <DialogTrigger asChild>
                <Button variant="danger" size="sm" disabled={!canWrite} title={writeTitle}>
                  Discard all dead letters
                </Button>
              </DialogTrigger>
              <DialogContent
                title={`Discard ${deadNoun}?`}
                description={`${deadCount === 1 ? "It is" : "They are"} deleted for good: ${name} is never sent ${deadCount === 1 ? "it" : "them"}. This cannot be undone. Transactions still waiting to be sent are not touched.`}
                footer={
                  <>
                    <DialogClose asChild>
                      <Button variant="secondary">Cancel</Button>
                    </DialogClose>
                    <DialogClose asChild>
                      <Button
                        variant="danger"
                        onClick={() =>
                          discard.mutate(
                            { id },
                            {
                              onSuccess: ({ discarded }) =>
                                toast({
                                  title:
                                    discarded === 0
                                      ? "Nothing was discarded: no dead letters were left"
                                      : `Discarded ${plural(discarded, "dead-lettered transaction")}`,
                                }),
                              onError: (err) =>
                                toast({
                                  title: "Couldn't discard the dead letters",
                                  description: problemDetail(err),
                                  variant: "danger",
                                }),
                            },
                          )
                        }
                      >
                        Discard all
                      </Button>
                    </DialogClose>
                  </>
                }
              />
            </Dialog>
          </div>
        </div>
      )}

      {items.length === 0 ? (
        <p className="text-sm text-text-muted">No pending or dead-lettered transactions.</p>
      ) : (
        <ul className="divide-y divide-border rounded-md border border-border">
          {items.map((entry) => (
            <BacklogRow
              key={entry.transaction_id}
              entry={entry}
              name={name}
              canWrite={canWrite}
              onDiscard={(transactionId) =>
                discard.mutate(
                  { id, transactionIds: [transactionId] },
                  {
                    onSuccess: ({ discarded }) =>
                      toast({
                        title:
                          discarded === 0
                            ? `Transaction ${transactionId} was not discarded: it is no longer dead-lettered`
                            : `Transaction ${transactionId} discarded`,
                      }),
                    onError: (err) =>
                      toast({
                        title: `Couldn't discard transaction ${transactionId}`,
                        description: problemDetail(err),
                        variant: "danger",
                      }),
                  },
                )
              }
            />
          ))}
        </ul>
      )}
    </div>
  );
}

/** One queue entry: state, id, when queued, attempts; what it carries and where; the last error. */
function BacklogRow({
  entry,
  name,
  canWrite,
  onDiscard,
}: {
  entry: AppServiceBacklogEntry;
  name: string;
  canWrite: boolean;
  onDiscard: (transactionId: string) => void;
}) {
  const transactionId = entry.transaction_id ?? "";
  const dead = entry.dead_lettered ?? false;
  const contents = describeBacklogContents(entry);
  const rooms = entry.room_ids ?? [];
  return (
    <li className="flex flex-col gap-1.5 px-4 py-3" data-testid={`backlog-entry-${transactionId}`}>
      <div className="flex flex-wrap items-center justify-between gap-3">
        <span className="flex flex-wrap items-center gap-x-3 gap-y-1">
          <Badge status={dead ? "danger" : "info"}>{dead ? "Dead-lettered" : "Waiting"}</Badge>
          <span className="font-identifier text-sm text-text-muted">{transactionId}</span>
          <span className="text-xs text-text-muted">
            {entry.enqueued_at ? (
              <>
                queued <RelativeTime at={entry.enqueued_at} />
              </>
            ) : (
              formatBacklogEntry(entry.age_ms ?? 0, dead)
            )}
          </span>
          <span className="text-xs text-text-muted">{plural(entry.attempts ?? 0, "attempt")}</span>
        </span>
        {dead && (
          <Dialog>
            <DialogTrigger asChild>
              <Button
                variant="ghost"
                size="sm"
                disabled={!canWrite}
                title={!canWrite ? "Needs bridges:write" : undefined}
                aria-label={`Discard transaction ${transactionId}`}
              >
                Discard
              </Button>
            </DialogTrigger>
            <DialogContent
              title={`Discard transaction ${transactionId}?`}
              description={`${contents ? `It carries ${contents}. ` : ""}It is deleted for good and ${name} is never sent it. This cannot be undone.`}
              footer={
                <>
                  <DialogClose asChild>
                    <Button variant="secondary">Cancel</Button>
                  </DialogClose>
                  <DialogClose asChild>
                    <Button variant="danger" onClick={() => onDiscard(transactionId)}>
                      Discard
                    </Button>
                  </DialogClose>
                </>
              }
            />
          </Dialog>
        )}
      </div>
      {(contents || rooms.length > 0) && (
        <div className="flex flex-wrap items-center gap-x-2 gap-y-1 text-xs text-text">
          {contents && <span>Carries {contents}</span>}{" "}
          {rooms.length > 0 && (
            <>
              <span className="text-text-muted">
                {rooms.length >= BACKLOG_ROOMS
                  ? `in these rooms and maybe more:`
                  : `in ${rooms.length === 1 ? "room" : "rooms"}`}
              </span>{" "}
              <ul className="flex flex-wrap gap-1.5" aria-label="Rooms">
                {rooms.map((room) => (
                  <li key={room}>
                    {/* Shortened on screen, whole on hover and to a screen reader. */}
                    <code
                      title={room}
                      className="block max-w-[16rem] truncate rounded-xs bg-surface-sunken px-1.5 py-0.5 font-identifier text-xs text-text-muted"
                    >
                      <span aria-hidden="true">{shortRoomId(room)}</span>
                      <span className="sr-only">{room}</span>
                    </code>
                  </li>
                ))}
              </ul>
            </>
          )}
        </div>
      )}
      {entry.last_error && (
        <p className="break-words text-xs text-danger">Last error: {entry.last_error}</p>
      )}
    </li>
  );
}
