import type { components } from "@/api/schema";
import { rooms } from "./rooms";
import { statisticsOverview } from "./dashboard";

type RoomStatistic = components["schemas"]["RoomStatistic"];
type UserMediaStatistic = components["schemas"]["UserMediaStatistic"];
type Timeseries = components["schemas"]["Timeseries"];

/**
 * The mock's statistics, following `crates/hs-admin/src/statistics.rs`: counters have a point
 * (possibly 0) for every step; gauges have one only where the server had a sample, and it began
 * sampling {@link SAMPLING_DAYS} days ago, so a longer range shows where history starts.
 */

const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const SAMPLING_DAYS = 21;
const MAX_POINTS = 1000;

export const COUNTERS = [
  "users.registered",
  "media.uploaded",
  "media.uploaded_bytes",
  "reports.received",
] as const;

export const GAUGES = [
  "users_count",
  "rooms_count",
  "daily_active_users",
  "monthly_active_users",
  "media_count",
  "media_bytes",
  "pending_reports_count",
  "federation_destinations_failing_count",
] as const;

export const roomStatistics: RoomStatistic[] = [
  ...rooms.map((r) => ({
    room_id: r.room_id,
    name: r.name ?? null,
    joined_members_count: r.joined_members_count ?? 0,
    state_events_count: r.state_events_count ?? 0,
  })),
  {
    room_id: "!announcements:example.org",
    name: "Announcements",
    joined_members_count: 598,
    state_events_count: 612,
  },
  {
    room_id: "!random:example.org",
    name: "Random",
    joined_members_count: 311,
    state_events_count: 340,
  },
  {
    room_id: "!support:example.org",
    name: "Support",
    joined_members_count: 87,
    state_events_count: 1_904,
  },
  { room_id: "!ops:example.org", name: null, joined_members_count: 4, state_events_count: 22 },
];

export const userMediaStatistics: UserMediaStatistic[] = [
  { user_id: "@alice:example.org", media_count: 1_204, media_bytes: 14.2 * 1024 ** 3 },
  { user_id: "@whatsapp_15551234:example.org", media_count: 2_310, media_bytes: 9.8 * 1024 ** 3 },
  { user_id: "@admin:example.org", media_count: 88, media_bytes: 1.1 * 1024 ** 3 },
  { user_id: "@spammer42:example.org", media_count: 640, media_bytes: 12.6 * 1024 ** 3 },
  { user_id: "@bot:example.org", media_count: 60, media_bytes: 4 * 1024 ** 2 },
];

/** Sorts a copy by `sort` (`field` or `-field`) from `allowed`, or answers why not. */
export function sortStatistics<T extends object>(
  items: T[],
  sort: string,
  allowed: readonly string[],
): T[] | { error: string } {
  const descending = sort.startsWith("-");
  const field = descending ? sort.slice(1) : sort;
  if (!allowed.includes(field)) {
    return { error: `sort by ${allowed.join(" or ")}, not "${sort}"` };
  }
  const key = (item: T) => Number((item as Record<string, unknown>)[field] ?? 0);
  return [...items].sort((a, b) => (descending ? key(b) - key(a) : key(a) - key(b)));
}

function parseStep(raw: string): number | null {
  if (/^\d+$/.test(raw)) return Number(raw) > 0 ? Number(raw) : null;
  const match = /^(\d+)([smhdw])$/.exec(raw);
  if (!match) return null;
  const unit = { s: 1000, m: MINUTE, h: HOUR, d: DAY, w: 7 * DAY }[match[2] as "s"];
  return Number(match[1]) * unit;
}

/** A stable pseudo-random number in [0, 1) for a bucket, so a chart does not jump between polls. */
function noise(metric: string, bucket: number): number {
  let h = 2166136261;
  for (const c of `${metric}:${bucket}`) h = Math.imul(h ^ c.charCodeAt(0), 16777619);
  return ((h >>> 0) % 10_000) / 10_000;
}

function counterValue(metric: string, at: number, stepMs: number): number {
  const perDay: Record<string, number> = {
    "users.registered": 4,
    "media.uploaded": 140,
    "media.uploaded_bytes": 1.3 * 1024 ** 3,
    "reports.received": 1.2,
  };
  const expected = (perDay[metric] ?? 1) * (stepMs / DAY);
  const hourOfDay = new Date(at).getUTCHours();
  const daytime =
    stepMs < DAY ? 0.3 + 1.4 * Math.max(0, Math.sin(((hourOfDay - 6) / 24) * 2 * Math.PI)) : 1;
  const value = expected * daytime * (0.4 + 1.2 * noise(metric, at));
  return Math.round(value);
}

function gaugeValue(metric: string, at: number, now: number): number {
  const daysAgo = (now - at) / DAY;
  const current = Number((statisticsOverview as Record<string, number | undefined>)[metric] ?? 0);
  const hourOfDay = new Date(at).getUTCHours();
  const wave = Math.sin(((hourOfDay - 8) / 24) * 2 * Math.PI);
  switch (metric) {
    case "users_count":
    case "rooms_count":
    case "media_count":
    case "media_bytes":
    case "monthly_active_users":
      return Math.max(
        0,
        Math.round(current * (1 - daysAgo * 0.004) * (1 + 0.002 * noise(metric, at))),
      );
    case "daily_active_users": {
      // A daily rhythm that ends at today's number, so the chart agrees with the Overview.
      const waveNow = Math.sin(((new Date(now).getUTCHours() - 8) / 24) * 2 * Math.PI);
      if (daysAgo < 0.3) return current;
      const jitter = 0.04 * (noise(metric, at) - 0.5);
      return Math.max(0, Math.round(current * (1 + 0.1 * (wave - waveNow) + jitter)));
    }
    default:
      return Math.max(
        0,
        Math.round(current + (noise(metric, at) > 0.8 ? 1 : 0) - (daysAgo > 2 ? 1 : 0)),
      );
  }
}

/** `GET /statistics/timeseries`, or the reason it is refused (the server's 400s). */
export function timeseries(
  query: URLSearchParams,
): Timeseries | { pointer: string; error: string } {
  const metric = query.get("metric") ?? "";
  const counter = (COUNTERS as readonly string[]).includes(metric);
  if (!counter && !(GAUGES as readonly string[]).includes(metric)) {
    return { pointer: "/metric", error: `unknown metric "${metric}"` };
  }
  const now = Date.now();
  const until = query.get("until") ? Date.parse(query.get("until")!) : now;
  const from = query.get("from") ? Date.parse(query.get("from")!) : until - 7 * DAY;
  if (Number.isNaN(until))
    return { pointer: "/until", error: "until must be an RFC 3339 date-time" };
  if (Number.isNaN(from)) return { pointer: "/from", error: "from must be an RFC 3339 date-time" };
  if (from >= until) return { pointer: "/from", error: "from must be before until" };
  const step = query.get("step") ? parseStep(query.get("step")!) : HOUR;
  if (!step || step < 1000) return { pointer: "/step", error: "step must be 15m, 1h, 1d..." };
  const start = Math.floor(from / step) * step;
  const end = Math.ceil(until / step) * step;
  if ((end - start) / step > MAX_POINTS) {
    return {
      pointer: "/step",
      error: `that is more than ${MAX_POINTS} points; ask for a longer step`,
    };
  }
  const samplingStart = now - SAMPLING_DAYS * DAY;
  const points: { at: string; value: number }[] = [];
  for (let at = start; at < end; at += step) {
    if (counter) {
      points.push({
        at: new Date(at).toISOString(),
        value: at > now ? 0 : counterValue(metric, at, step),
      });
    } else if (at + step > samplingStart && at <= now) {
      points.push({ at: new Date(at).toISOString(), value: gaugeValue(metric, at, now) });
    }
  }
  return { metric, step_ms: step, points };
}
