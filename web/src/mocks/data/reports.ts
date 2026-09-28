import type { components } from "@/api/schema";
import { publishMockEvent } from "./events";

type Report = components["schemas"]["Report"];
type ReportResolve = components["schemas"]["ReportResolve"];

/**
 * The mock's reports, shaped like `crates/hs-admin/src/reports.rs`'s `AdminReport`: every
 * optional field present (as `null` when absent), `event` only on `GET /reports/{id}` and only
 * for an event report whose event the server holds. Ids are ULID-shaped and sort by time, so
 * newest-first is reverse id order, as on the server.
 */

const HOUR = 3_600_000;
const iso = (msAgo: number) => new Date(Date.now() - msAgo).toISOString();

interface MockReport extends Report {
  /** The event as the room holds it, returned only by `GET /reports/{id}`. */
  heldEvent?: Report["event"];
}

function seed(): MockReport[] {
  const base = {
    room_id: null,
    event_id: null,
    reported_user_id: null,
    reason: null,
    score: null,
    resolution: null,
    resolution_note: null,
    resolved_at: null,
    resolved_by: null,
    event: null,
  } satisfies Partial<Report>;
  return [
    {
      ...base,
      id: "01J9ZQ0000000000000000R008",
      kind: "event",
      status: "open",
      room_id: "!general:example.org",
      event_id: "$spam-link-1",
      reporter_id: "@alice:example.org",
      reported_user_id: "@spammer42:example.org",
      reason: "Scam link, posted in three rooms",
      score: -100,
      received_at: iso(0.4 * HOUR),
      heldEvent: {
        event_id: "$spam-link-1",
        type: "m.room.message",
        sender: "@spammer42:example.org",
        origin_server_ts: Date.now() - 0.5 * HOUR,
        redacted: false,
        content: {
          msgtype: "m.text",
          body: "Free crypto!!! Claim yours at https://totally-legit.example/claim",
        } as unknown as Record<string, never>,
      },
    },
    {
      ...base,
      id: "01J9ZQ0000000000000000R007",
      kind: "user",
      status: "open",
      reporter_id: "@alice:example.org",
      reported_user_id: "@spammer42:example.org",
      reason: "Sends me unsolicited invites every hour",
      received_at: iso(2 * HOUR),
    },
    {
      ...base,
      id: "01J9ZQ0000000000000000R006",
      kind: "room",
      status: "open",
      room_id: "!spam-central:example.org",
      reporter_id: "@whatsapp_15551234:example.org",
      reason: "This whole room is advertising",
      received_at: iso(5 * HOUR),
    },
    {
      ...base,
      id: "01J9ZQ0000000000000000R005",
      kind: "event",
      status: "open",
      room_id: "!general:example.org",
      event_id: "$gone-event",
      reporter_id: "@admin:example.org",
      reported_user_id: "@spammer42:example.org",
      reason: null,
      received_at: iso(26 * HOUR),
      heldEvent: {
        event_id: "$gone-event",
        type: "m.room.message",
        sender: "@spammer42:example.org",
        origin_server_ts: Date.now() - 27 * HOUR,
        redacted: true,
        content: {} as Record<string, never>,
      },
    },
    {
      ...base,
      id: "01J9ZQ0000000000000000R004",
      kind: "event",
      status: "resolved",
      room_id: "!spam-central:example.org",
      event_id: "$abuse-1",
      reporter_id: "@alice:example.org",
      reported_user_id: "@spammer42:example.org",
      reason: "Abusive message",
      score: -80,
      received_at: iso(3 * 24 * HOUR),
      resolution: "redacted",
      resolution_note: "Redacted and warned.",
      resolved_at: iso(3 * 24 * HOUR - 2 * HOUR),
      resolved_by: "@admin:example.org",
    },
    {
      ...base,
      id: "01J9ZQ0000000000000000R003",
      kind: "user",
      status: "dismissed",
      reporter_id: "@spammer42:example.org",
      reported_user_id: "@alice:example.org",
      reason: "She reported me",
      received_at: iso(4 * 24 * HOUR),
      resolution: "no_action",
      resolution_note: "Retaliation for a valid report.",
      resolved_at: iso(4 * 24 * HOUR - HOUR),
      resolved_by: "@admin:example.org",
    },
  ];
}

let reports = seed();

/** Puts the reports back as they were (Vitest runs this after every test). */
export function resetReports(): void {
  reports = seed();
}

function wire(report: MockReport, withEvent: boolean): Report {
  const { heldEvent, ...rest } = report;
  return { ...rest, event: withEvent ? (heldEvent ?? null) : null };
}

export function listReports(query: URLSearchParams): Report[] | { error: string } {
  const kind = query.get("kind");
  const status = query.get("status");
  const roomId = query.get("room_id");
  const reportedUserId = query.get("reported_user_id");
  const reporterId = query.get("reporter_id");
  const sort = query.get("sort") ?? "-received_at";
  const items = reports.filter(
    (r) =>
      (!kind || r.kind === kind) &&
      (!status || r.status === status) &&
      (!roomId || r.room_id === roomId) &&
      (!reportedUserId || r.reported_user_id === reportedUserId) &&
      (!reporterId || r.reporter_id === reporterId),
  );
  switch (sort) {
    case "-received_at":
      items.sort((a, b) => b.id.localeCompare(a.id));
      break;
    case "received_at":
      items.sort((a, b) => a.id.localeCompare(b.id));
      break;
    case "score":
      items.sort(
        (a, b) => (a.score ?? Number.MAX_SAFE_INTEGER) - (b.score ?? Number.MAX_SAFE_INTEGER),
      );
      break;
    case "-score":
      items.sort(
        (a, b) => (b.score ?? Number.MIN_SAFE_INTEGER) - (a.score ?? Number.MIN_SAFE_INTEGER),
      );
      break;
    default:
      return {
        error: `reports sort by received_at or score (prefix - for descending), not "${sort}"`,
      };
  }
  return items.map((r) => wire(r, false));
}

export function getReport(id: string): Report | undefined {
  const report = reports.find((r) => r.id === id);
  return report && wire(report, true);
}

/** `POST /reports/{id}/resolve`: `undefined` for an unknown id, `"closed"` for one already closed. */
export function resolveReport(
  id: string,
  body: ReportResolve,
  by: string,
): Report | "closed" | undefined {
  const report = reports.find((r) => r.id === id);
  if (!report) return undefined;
  if (report.status !== "open") return "closed";
  report.status = body.resolution === "no_action" ? "dismissed" : "resolved";
  report.resolution = body.resolution;
  report.resolution_note = body.note?.trim() ? body.note : null;
  report.resolved_at = new Date().toISOString();
  report.resolved_by = by;
  return wire(report, true);
}

export function deleteReport(id: string): boolean {
  const before = reports.length;
  reports = reports.filter((r) => r.id !== id);
  return reports.length < before;
}

/**
 * Files a report as somebody would through the client-server API, and publishes it as
 * `report.created` on the mock event stream, as the server does.
 */
export function fileMockReport(report: Omit<Report, "status" | "received_at" | "id">): Report {
  const filed: MockReport = {
    ...report,
    id: `01J9ZQ${String(Date.now()).padStart(20, "0")}`,
    status: "open",
    received_at: new Date().toISOString(),
  };
  reports = [...reports, filed];
  const wired = wire(filed, false);
  publishMockEvent("report.created", wired, { type: "report", id: filed.id });
  return wired;
}

export function openReportCount(): number {
  return reports.filter((r) => r.status === "open").length;
}
