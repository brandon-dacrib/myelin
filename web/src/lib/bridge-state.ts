import type { AppService, AppServiceHealthStatus, AppServiceQueue } from "@/api/bridges";
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
 * `AppService.queue` in words for the bridges list: "Up to date", or how many transactions wait
 * and how long the oldest has waited, and how many ran out of attempts. The colour says whether
 * to look: a bridge whose oldest transaction has waited over a minute is falling behind, and a
 * dead-lettered one needs a replay from its page. A server older than OpenAPI 0.1.11 sends no
 * queue, which reads as a dash.
 */
export function describeQueue(queue: AppServiceQueue | undefined): {
  text: string;
  status: NonNullable<BadgeProps["status"]>;
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
    parts.push(`${queue.dead_lettered} failed`);
    status = "danger";
  }
  return parts.length === 0 ? { text: "Up to date", status } : { text: parts.join(" · "), status };
}
