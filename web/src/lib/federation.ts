/**
 * Words and small calculations for the Federation pages (`pages/FederationPage.tsx`,
 * `pages/FederationDestinationPage.tsx`): how a destination is doing, in the operator's words.
 */
import type { Destination, DestinationForgotten } from "@/api/federation";
import { formatCount } from "@/lib/format";

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

/**
 * The configuration row that holds how long a destination sharing no room is kept before the
 * hourly sweep forgets it (`federation.forget_unused_destinations_after`, decision 0042).
 */
export const FORGET_AFTER_SETTING = "forget_unused_destinations_after";

/** Its default, as the server writes durations ("1w"). */
export const DEFAULT_FORGET_AFTER = "1w";

/**
 * A `Duration` setting as the server gives it (`hs_config::Duration`: a string like `"1w"` or
 * `"36h"`, or a count of milliseconds), as a person says it. `null` for "off" (`0`), `undefined`
 * when the value is not a duration at all.
 */
export function formatSettingDuration(value: unknown): string | null | undefined {
  if (typeof value === "number") {
    if (value <= 0) return null;
    return formatDurationWords(value);
  }
  if (typeof value !== "string") return undefined;
  const ms = parseDurationMs(value);
  if (ms === undefined) return undefined;
  return ms === 0 ? null : formatDurationWords(ms);
}

const UNIT_MS: Record<string, number> = {
  ms: 1,
  s: 1000,
  m: 60_000,
  h: 3_600_000,
  d: 86_400_000,
  w: 7 * 86_400_000,
  y: 365 * 86_400_000,
};

/** `"1w"` → 604,800,000; `"1h30m"` → 5,400,000; `"0"` → 0; `undefined` for anything else. */
export function parseDurationMs(text: string): number | undefined {
  const trimmed = text.trim();
  if (/^\d+$/.test(trimmed)) return Number(trimmed);
  const pattern = /(\d+)(ms|s|m|h|d|w|y)/gy;
  let total = 0;
  let consumed = 0;
  for (const match of trimmed.matchAll(pattern)) {
    total += Number(match[1]) * UNIT_MS[match[2]];
    consumed = match.index + match[0].length;
  }
  return consumed === trimmed.length && trimmed.length > 0 ? total : undefined;
}

/** `604_800_000` → "1 week", `172_800_000` → "2 days", `5_400_000` → "1.5 hours". */
export function formatDurationWords(ms: number): string {
  const unit = (value: number, name: string) => {
    const rounded = Math.round(value * 10) / 10;
    return `${rounded} ${name}${rounded === 1 ? "" : "s"}`;
  };
  if (ms >= UNIT_MS.w && ms % UNIT_MS.w === 0) return unit(ms / UNIT_MS.w, "week");
  if (ms >= UNIT_MS.d) return unit(ms / UNIT_MS.d, "day");
  return formatInterval(ms);
}

/**
 * Why a prune forgot or kept a destination (`DestinationPruneEntry.reason`,
 * `crates/hs-admin/src/federation.rs`), as a short label an operator reads in a list.
 */
export const PRUNE_REASON_LABELS: Record<string, string> = {
  unused: "Shares no room, nothing queued",
  failing: "Failing, queue only for rooms this server left",
  shares_rooms: "Shares a room",
  queued_for_current_rooms: "Events queued for a room this server is in",
  queued_not_failing: "Has a queue and is not failing",
  failing_recently: "Not failing for long enough",
  active_recently: "Had activity recently",
};

/** The label for a prune reason, or the reason itself for one this page does not know. */
export function pruneReasonLabel(reason: string): string {
  return PRUNE_REASON_LABELS[reason] ?? reason;
}

/** "3 events" / "1 event", for a sentence. */
export function things(count: number, singular: string, plural = `${singular}s`): string {
  return `${formatCount(count)} ${count === 1 ? singular : plural}`;
}

/**
 * One sentence of what forgetting `d` drops, from the numbers the list already has: the whole
 * truth is in the server's answer afterwards (`DestinationForgotten`).
 */
export function forgetConsequence(d: Destination): string {
  const pdus = d.pending_pdu_count ?? 0;
  const edus = d.pending_edu_count ?? 0;
  const parts: string[] = [];
  if (pdus > 0) parts.push(`${things(pdus, "queued event")} (unsent, lost)`);
  if (edus > 0) parts.push(`${things(edus, "queued message")} (typing, receipts, device updates)`);
  if (d.catch_up_since) parts.push("the note that it is behind and must be caught up");
  parts.push("its retry state", "its cached signing keys");
  const list =
    parts.length === 1
      ? parts[0]
      : `${parts.slice(0, -1).join(", ")} and ${parts[parts.length - 1]}`;
  return `This drops ${list}. It is learned again from nothing the next time a room brings the two servers together.`;
}

/** What the server dropped, for a toast after a forget. */
export function forgottenSummary(result: DestinationForgotten): string {
  const parts: string[] = [];
  if (result.dropped_pdu_count > 0) parts.push(things(result.dropped_pdu_count, "event"));
  if (result.dropped_edu_count > 0) parts.push(things(result.dropped_edu_count, "message"));
  if (result.dropped_key_count > 0) parts.push(things(result.dropped_key_count, "signing key"));
  return parts.length === 0 ? "Nothing was queued for it." : `Dropped ${parts.join(", ")}.`;
}
