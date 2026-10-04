/**
 * Words and small calculations for the Federation pages (`pages/FederationPage.tsx`,
 * `pages/FederationDestinationPage.tsx`): how a destination is doing, in the operator's words.
 */
import type { Destination } from "@/api/federation";

type BadgeStatus = "success" | "warning" | "danger" | "info";

/** The default of `federation.max_queued_pdus_per_destination` (`hs_config::FederationConfig`). */
export const DEFAULT_MAX_QUEUED_PDUS = 10_000;

/** The configuration row that holds the queue limit, for a link from the explanation. */
export const MAX_QUEUED_PDUS_SETTING = "max_queued_pdus_per_destination";

/**
 * The fields `GET /federation/destinations` sorts by (`DESTINATION_SORT_FIELDS` in
 * `crates/hs-admin/src/router.rs`); `-` in front for descending. A destination without the
 * timestamp sorts last either way. Without `sort`, failing destinations come first, then the
 * rest by name.
 */
export const DESTINATION_SORT_FIELDS = [
  "server_name",
  "failing_since",
  "last_successful_at",
  "retry_last_at",
  "pending_pdu_count",
  "pending_edu_count",
] as const;

/** How many destinations the Federation page asks for at a time. */
export const DESTINATION_PAGE_SIZE = 50;

export interface DestinationHealth {
  status: BadgeStatus;
  label: string;
  /** One sentence: what this state means for the operator. */
  explanation: string;
}

/**
 * How sending to a destination is going: failing (its current run of failures has a start),
 * backing off (a retry interval is set: the last attempt failed and the next waits), or healthy.
 */
export function destinationHealth(d: Destination): DestinationHealth {
  if (d.failing_since) {
    return {
      status: "danger",
      label: "Failing",
      explanation:
        "Every attempt to send to this server has failed since the time shown. Its events wait here and are sent when it answers again; nothing is lost.",
    };
  }
  if (d.retry_interval_ms) {
    return {
      status: "warning",
      label: "Backing off",
      explanation:
        "The last attempt failed, so this server waits before trying again, a little longer after each failure. Reset backoff to try at once.",
    };
  }
  return {
    status: "success",
    label: "Healthy",
    explanation: "The last attempt to send to this server succeeded.",
  };
}

/** Attention first: failing, then catching up or backing off, then healthy. */
export function destinationSeverity(d: Destination): number {
  const health = destinationHealth(d);
  if (health.status === "danger") return 0;
  if (d.catch_up_since) return 1;
  if (health.status === "warning") return 2;
  return 3;
}

/**
 * When the next attempt is due: the last attempt plus the retry interval, and whether that time
 * has already come (the next attempt then waits only for something to send). `null` while the
 * destination is not backing off, or when the server has not said when it last tried.
 */
export function nextAttemptAt(
  d: Destination,
  now: number = Date.now(),
): { at: string; due: boolean } | null {
  if (!d.retry_last_at || !d.retry_interval_ms) return null;
  const last = Date.parse(d.retry_last_at);
  if (Number.isNaN(last)) return null;
  const at = last + d.retry_interval_ms;
  return { at: new Date(at).toISOString(), due: at <= now };
}

/** `90_000` → `"1.5 minutes"`, `3_600_000` → `"1 hour"`: a retry interval as a person says it. */
export function formatInterval(ms: number): string {
  const seconds = ms / 1000;
  const unit = (value: number, name: string) => {
    const rounded = Math.round(value * 10) / 10;
    return `${rounded} ${name}${rounded === 1 ? "" : "s"}`;
  };
  if (seconds < 60) return unit(seconds, "second");
  if (seconds < 3600) return unit(seconds / 60, "minute");
  if (seconds < 86_400) return unit(seconds / 3600, "hour");
  return unit(seconds / 86_400, "day");
}

/**
 * What catch-up is, in one paragraph, with the queue limit the server runs with. Shared by the
 * destination page and the list's explanation.
 */
export function catchUpExplanation(limit: number): string {
  return `This server was unreachable for longer than its queue holds (${limit.toLocaleString("en-US")} events). Rather than keep every event for it, this server stopped queuing and remembers only which rooms it is behind in. When it answers again it is sent the latest event of each of those rooms, and it fetches the history in between itself, as any server does after an outage. Nothing is lost; the gap just fills from the other side.`;
}
