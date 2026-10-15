import type {
  AppService,
  AppServiceBacklogEntry,
  AppServiceHealthStatus,
  AppServiceQueue,
} from "@/api/bridges";
import { formatDuration } from "@/lib/format";
import type { BadgeProps } from "@/components/ui/badge/Badge";

/**
 * Maps `AppService.health` (`crates/hs-admin/openapi/openapi.yaml`:
 * `healthy | degraded | down | paused | unknown`) to a Badge status + label.
 * Reconciled 2026-09-18: earlier drafts of this file used a richer, mautrix
 * -shaped state vocabulary (`waiting_for_ping`, `bridge_unreachable`, ...)
 * that does not exist on the real `AppService` resource; the real health
 * enum is coarser, so this mapping is coarser too.
 */
export const bridgeHealthMeta: Record<
  AppServiceHealthStatus,
  { status: NonNullable<BadgeProps["status"]>; label: string }
> = {
  healthy: { status: "success", label: "Healthy" },
  degraded: { status: "warning", label: "Degraded" },
  down: { status: "danger", label: "Down" },
  paused: { status: "muted", label: "Paused" },
  unknown: { status: "info", label: "Unknown" },
};

/**
 * The health key to look up in `bridgeHealthMeta`: `paused` (a client-side
 * override, since a paused appservice can still report a stale `health`
 * from before it was paused) falling back to `health ?? "unknown"` (the
 * field is optional on the schema).
 */
export function healthKeyOf(
  appservice: Pick<AppService, "health" | "paused">,
): AppServiceHealthStatus {
  if (appservice.paused) return "paused";
  return appservice.health ?? "unknown";
}

export function formatBacklogEntry(ageMs: number, deadLettered: boolean): string {
  const seconds = ageMs / 1000;
  const age =
    seconds >= 3600 ? `${Math.round(seconds / 3600)} h` : `${Math.round(seconds / 60)} min`;
  return deadLettered ? `Dead-lettered, ${age} old` : `Pending, ${age} old`;
}

/** Past this, a bridge's oldest waiting transaction is worth a warning colour in the list. */
const QUEUE_BEHIND_MS = 60_000;

/**
 * What "dead-lettered" means, in one sentence, wherever a count of them is shown (the bridges
 * list's queue cell and its column help, the bridge page). The retry budget is the scheduler's
 * (`hs_appservice::scheduler`): ten refusals, or a day of no answer, timeouts, 5xx, 408 or 429.
 */
export const DEAD_LETTER_HINT =
  "Dead-lettered: the server gave up on them after the bridge refused them 10 times, or could not be reached for a whole day. They are kept until someone replays or discards them on the bridge's page.";

/**
 * `AppService.queue` in words for the bridges list: "Up to date", or how many transactions wait
 * and how long the oldest has waited, and how many were dead-lettered (their retry budget ran
 * out). The colour says whether to look: a bridge whose oldest transaction has waited over a
 * minute is falling behind, and a dead-lettered one waits for an operator to replay or discard
 * it from the bridge's page, which `hint` says. A server older than OpenAPI 0.1.11 sends no
 * queue, which reads as a dash.
 */
export function describeQueue(queue: AppServiceQueue | undefined): {
  text: string;
  status: NonNullable<BadgeProps["status"]>;
  /** Set when there are dead letters: what they are and where they are dealt with. */
  hint?: string;
} {
  if (!queue) return { text: "—", status: "neutral" };
  const parts: string[] = [];
  let status: NonNullable<BadgeProps["status"]> = "success";
  if (queue.pending > 0) {
    const age = queue.oldest_pending_age_ms ?? 0;
    parts.push(`${queue.pending} waiting, oldest ${formatDuration(age)}`);
    status = age > QUEUE_BEHIND_MS ? "warning" : "neutral";
  }
  if (queue.dead_lettered > 0) {
    parts.push(`${queue.dead_lettered} dead-lettered`);
    status = "danger";
  }
  if (parts.length === 0) return { text: "Up to date", status };
  return {
    text: parts.join(" · "),
    status,
    ...(queue.dead_lettered > 0 ? { hint: DEAD_LETTER_HINT } : {}),
  };
}

/**
 * What one queue entry carries, in words: "12 events · 3 ephemeral · 1 to-device", leaving out
 * what it has none of. Empty when it carries nothing countable, or the server is older than
 * OpenAPI 0.1.18 and does not say.
 */
export function describeBacklogContents(
  entry: Pick<AppServiceBacklogEntry, "events" | "ephemeral" | "to_device">,
): string {
  const parts: string[] = [];
  const events = entry.events ?? 0;
  const ephemeral = entry.ephemeral ?? 0;
  const toDevice = entry.to_device ?? 0;
  if (events > 0) parts.push(`${events} ${events === 1 ? "event" : "events"}`);
  if (ephemeral > 0) parts.push(`${ephemeral} ephemeral`);
  if (toDevice > 0) parts.push(`${toDevice} to-device`);
  return parts.join(" · ");
}

/**
 * A room id short enough for a chip that still says where the room lives: the opaque part cut
 * after its first six characters, the server kept (`!OGEhHVWSdvArJzumhm:matrix.org` →
 * `!OGEhHV…:matrix.org`). An id whose opaque part is a dozen characters or fewer is left alone.
 */
export function shortRoomId(roomId: string): string {
  const colon = roomId.indexOf(":");
  if (colon < 0 || colon <= 12) return roomId;
  return `${roomId.slice(0, 7)}…${roomId.slice(colon)}`;
}

/**
 * The oldest of the dead-lettered entries' ages, in milliseconds, or undefined when there are
 * none. The backlog is read oldest first, so the first page holds the oldest entries.
 */
export function oldestDeadLetterAgeMs(
  entries: Pick<AppServiceBacklogEntry, "age_ms" | "dead_lettered">[],
): number | undefined {
  const ages = entries.filter((e) => e.dead_lettered).map((e) => e.age_ms ?? 0);
  return ages.length === 0 ? undefined : Math.max(...ages);
}
