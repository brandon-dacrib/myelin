import type { RoomEvent, RoomTask } from "@/api/room-contents";

/** How the room page words events and the results of the tasks it starts. */

/** A one-line summary of an event, as an operator scanning a timeline would want it. */
export function summarizeEvent(
  event: Pick<RoomEvent, "type" | "content" | "state_key" | "redacted">,
): string {
  if (event.redacted) return "(redacted)";
  const content = (event.content ?? {}) as Record<string, unknown>;
  const text = (key: string) =>
    typeof content[key] === "string" ? (content[key] as string) : null;
  switch (event.type) {
    case "m.room.message":
      return text("body") ?? `(${text("msgtype") ?? "message"})`;
    case "m.room.member":
      return `${event.state_key ?? "someone"}: ${text("membership") ?? "?"}`;
    case "m.room.name":
      return `name: ${text("name") ?? ""}`;
    case "m.room.topic":
      return `topic: ${text("topic") ?? ""}`;
    case "m.room.encrypted":
      return "(encrypted)";
    default:
      return event.state_key != null ? `state (${event.state_key || '""'})` : "";
  }
}

function num(result: Record<string, unknown>, key: string): number {
  return typeof result[key] === "number" ? (result[key] as number) : 0;
}

function list(result: Record<string, unknown>, key: string): string[] {
  return Array.isArray(result[key]) ? (result[key] as unknown[]).map(String) : [];
}

/** What a finished room task did, in a sentence; `null` when it has not succeeded. */
export function describeRoomTaskResult(
  task: Pick<RoomTask, "action" | "status" | "result">,
): string | null {
  if (task.status !== "succeeded") return null;
  const result = (task.result ?? {}) as Record<string, unknown>;
  switch (task.action) {
    case "rooms.purge_history": {
      const purged = num(result, "purged");
      return `Purged ${purged.toLocaleString()} ${purged === 1 ? "event" : "events"}; kept ${num(result, "kept_state").toLocaleString()} state and ${num(result, "kept_local").toLocaleString()} local.`;
    }
    case "rooms.delete": {
      const kicked = list(result, "kicked_users").length;
      const failed = list(result, "failed_to_kick_users").length;
      const parts = [`${kicked} ${kicked === 1 ? "member" : "members"} removed`];
      if (failed) parts.push(`${failed} could not be removed`);
      if (result.new_room_id) parts.push(`moved to ${String(result.new_room_id)}`);
      if (result.blocked) parts.push("blocked");
      if (result.purged)
        parts.push(`${num(result, "events_deleted").toLocaleString()} events deleted`);
      return `${parts.join(", ")}.`;
    }
    case "rooms.media.quarantine": {
      const q = num(result, "quarantined");
      return `Quarantined ${q} ${q === 1 ? "item" : "items"}; ${num(result, "already_quarantined")} already were, ${num(result, "protected")} protected.`;
    }
    default:
      return "Done.";
  }
}

/** A `<input type="datetime-local">` value (local time, no zone) as RFC 3339 UTC, or `null`. */
export function localInputToRfc3339(value: string): string | null {
  if (!value) return null;
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? null : date.toISOString();
}

/** `m.room.join_rules`' rule, in words. */
export const JOIN_RULE_LABELS: Record<string, string> = {
  public: "Anyone can join",
  invite: "Invite only",
  knock: "Anyone can ask to join",
  restricted: "Members of certain spaces can join",
  knock_restricted: "Members of certain spaces can join; anyone can ask",
  private: "Invite only",
};

/** `m.room.history_visibility`, in words: who can read the room's history. */
export const HISTORY_VISIBILITY_LABELS: Record<string, string> = {
  world_readable: "Anyone, even without joining",
  shared: "Members, including history from before they joined",
  invited: "Members, from when they were invited",
  joined: "Members, from when they joined",
};

/** `m.room.guest_access`, in words: whether guests (accounts with no password) may join. */
export const GUEST_ACCESS_LABELS: Record<string, string> = {
  can_join: "Guests may join",
  forbidden: "Guests may not join",
};

/** A membership state (`m.room.member`'s `membership`), in words. */
export const MEMBERSHIP_LABELS: Record<string, string> = {
  join: "Joined",
  invite: "Invited",
  knock: "Asked to join",
  leave: "Left",
  ban: "Banned",
};

/** A wire value in words: its label, or the value with its underscores made spaces. */
export function roomWords(
  labels: Record<string, string>,
  value: string | null | undefined,
): string {
  if (!value) return "—";
  return labels[value] ?? value.replaceAll("_", " ");
}
