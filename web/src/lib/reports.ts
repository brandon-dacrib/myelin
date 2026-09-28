import type { Report, ReportKind, ReportResolution, ReportStatus } from "@/api/reports";

/**
 * How the interface words a report (docs/design/information-architecture.md, Reports: "What
 * have users flagged, and what did we do about it?").
 */

type BadgeStatus = "success" | "warning" | "danger" | "info" | "muted" | "neutral";

export const REPORT_STATUS_META: Record<ReportStatus, { label: string; status: BadgeStatus }> = {
  open: { label: "Open", status: "warning" },
  resolved: { label: "Resolved", status: "success" },
  dismissed: { label: "Dismissed", status: "muted" },
};

export const REPORT_KIND_LABELS: Record<ReportKind, string> = {
  event: "Message",
  room: "Room",
  user: "User",
};

/**
 * The resolutions the contract allows, in the order a moderator weighs them, each with what it
 * records. Recording one does not act on its own: the page says so, and links to where the
 * action lives.
 */
export const RESOLUTIONS: readonly { value: ReportResolution; label: string; hint: string }[] = [
  {
    value: "no_action",
    label: "No action (dismiss)",
    hint: "Nothing needed doing. The report is dismissed.",
  },
  { value: "warned", label: "Warned the user", hint: "You warned whoever was reported." },
  { value: "redacted", label: "Redacted the content", hint: "The reported message was removed." },
  { value: "suspended", label: "Suspended the user", hint: "They can read but not send." },
  {
    value: "deactivated",
    label: "Deactivated the user",
    hint: "The account is closed for good.",
  },
  {
    value: "room_blocked",
    label: "Blocked the room",
    hint: "Nobody on this server can join it.",
  },
  { value: "other", label: "Something else", hint: "Say what in the note." },
];

export function resolutionLabel(resolution: ReportResolution | null | undefined): string {
  if (!resolution) return "—";
  return RESOLUTIONS.find((r) => r.value === resolution)?.label ?? resolution;
}

/** A one-line reading of what was reported: "Message in General", "User @bob:example.org". */
export function reportSubject(report: Report, roomName?: string | null): string {
  switch (report.kind) {
    case "event":
      return `Message in ${roomName ?? report.room_id ?? "a room"}`;
    case "room":
      return `Room ${roomName ?? report.room_id ?? ""}`.trim();
    case "user":
      return `User ${report.reported_user_id ?? ""}`.trim();
  }
}

/**
 * The reporter's score as words. Clients that still send one use -100 (most offensive) to 0
 * (inoffensive); most send none.
 */
export function describeScore(score: number | null | undefined): string {
  if (score == null) return "No score";
  if (score <= -75) return `${score} (very offensive)`;
  if (score <= -25) return `${score} (offensive)`;
  if (score < 0) return `${score} (mildly offensive)`;
  return `${score} (inoffensive)`;
}
