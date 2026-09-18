import type { AppService, AppServiceHealthStatus } from "@/api/bridges";
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
