/**
 * The mock's moderation-and-activity state for users: rate-limit overrides, support sessions
 * minted by "login as", per-user statistics, memberships (read from the rooms' member lists)
 * and the two per-user Tasks, redacting their events and deleting their media. Shaped like
 * `crates/hs-admin/openapi/openapi.yaml`'s Users operations. Mutable; `src/test/setup.ts`
 * calls {@link resetUserModeration} between tests.
 */
import type { components } from "@/api/schema";
import { users, userDevices, findUser } from "./users";
import { rooms, roomMembers } from "./rooms";
import { mediaItems, removeMedia } from "./media";
import { putDrivenTask, recordFinishedTask } from "./tasks";

type Task = components["schemas"]["Task"];
type Session = components["schemas"]["Session"];
type RoomMember = components["schemas"]["RoomMember"];
type RateLimitOverride = components["schemas"]["RateLimitOverride"];

/** How long a mock redaction takes, so a page polling it sees it move. */
export const REDACTION_RUN_MS = 3_000;

/** How many events each user has sent, as the mock tells it. */
const EVENTS_SENT: Record<string, number> = {
  "@admin:example.org": 310,
  "@alice:example.org": 1_204,
  "@spammer42:example.org": 12,
  "@whatsapp_15551234:example.org": 88,
  "@bot:example.org": 3,
};

interface State {
  rateLimits: Record<string, RateLimitOverride>;
  supportSessions: Record<string, Session[]>;
  /** Events already redacted, per user, so a second run finds fewer. */
  redacted: Record<string, number>;
  /** The flags as seeded, to put back. */
  flags: Record<string, { suspended?: boolean; shadow_banned?: boolean; deactivated?: boolean }>;
  devices: Record<string, number>;
}

function seed(): State {
  return {
    rateLimits: { "@whatsapp_15551234:example.org": { messages_per_second: 0, burst_count: 10 } },
    supportSessions: {},
    redacted: {},
    flags: Object.fromEntries(
      users.map((u) => [
        u.user_id,
        { suspended: u.suspended, shadow_banned: u.shadow_banned, deactivated: u.deactivated },
      ]),
    ),
    devices: Object.fromEntries(Object.entries(userDevices).map(([k, v]) => [k, v.length])),
  };
}

let state = seed();

/** Puts every user's moderation flags and this module's state back as they were. */
export function resetUserModeration(): void {
  for (const [userId, flags] of Object.entries(state.flags)) {
    const user = findUser(userId);
    if (user) Object.assign(user, flags);
  }
  for (const [userId, count] of Object.entries(state.devices)) {
    userDevices[userId]?.splice(count);
  }
  state = seed();
}

export function getRateLimit(userId: string): RateLimitOverride {
  return state.rateLimits[userId] ?? {};
}

export function setRateLimit(userId: string, override: RateLimitOverride): RateLimitOverride {
  state.rateLimits[userId] = override;
  return override;
}

export function clearRateLimit(userId: string): void {
  delete state.rateLimits[userId];
}

/** Mints a support session: a new device, marked as one, with a token shown once. */
export function mintSupportSession(userId: string, validForSeconds: number, now = Date.now()) {
  const deviceId = `SUPPORT${Math.random().toString(36).slice(2, 8).toUpperCase()}`;
  const createdAt = new Date(now).toISOString();
  const session: Session = {
    device_id: deviceId,
    display_name: null,
    ip: null,
    user_agent: null,
    created_at: createdAt,
    last_seen_at: null,
    support_session: true,
  };
  (state.supportSessions[userId] ??= []).push(session);
  (userDevices[userId] ??= []).push({
    device_id: deviceId,
    display_name: null,
    last_seen_ip: null,
    last_seen_at: null,
  });
  return {
    user_id: userId,
    access_token: `mock_support_${Math.random().toString(36).slice(2, 14)}`,
    device_id: deviceId,
    expires_at: new Date(now + validForSeconds * 1000).toISOString(),
  };
}

const AGENTS: Record<string, string> = {
  WEBDEV1: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) Firefox/131.0",
  MOBILE1: "Element/1.11 (iPhone; iOS 18.0)",
  ALICEWEB: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/129.0",
  SPAMDEV: "curl/8.7.1",
};

/** Every session the user has: their devices, with support sessions marked. */
export function listSessions(userId: string): Session[] {
  const support = new Set((state.supportSessions[userId] ?? []).map((s) => s.device_id));
  return (userDevices[userId] ?? []).map((d) => {
    const minted = state.supportSessions[userId]?.find((s) => s.device_id === d.device_id);
    return (
      minted ?? {
        device_id: d.device_id,
        display_name: d.display_name ?? null,
        ip: d.last_seen_ip ?? null,
        user_agent: AGENTS[d.device_id] ?? null,
        last_seen_at: d.last_seen_at ?? null,
        support_session: support.has(d.device_id),
      }
    );
  });
}

/** One row per room the user has a membership in, from the rooms' member lists. */
export function listMemberships(userId: string, membership?: string | null): RoomMember[] {
  return Object.entries(roomMembers).flatMap(([roomId, members]) => {
    const m = members.find((x) => x.user_id === userId);
    if (!m || (membership && m.membership !== membership)) return [];
    const room = rooms.find((r) => r.room_id === roomId);
    return [{ ...m, room_id: roomId, room_name: room?.name ?? null }];
  });
}

export function userMedia(userId: string) {
  return mediaItems.filter((m) => m.origin === "local" && m.uploader === userId);
}

function eventsLeft(userId: string): number {
  return Math.max((EVENTS_SENT[userId] ?? 0) - (state.redacted[userId] ?? 0), 0);
}

export function userStatistics(userId: string) {
  const media = userMedia(userId);
  return {
    user_id: userId,
    joins_count: listMemberships(userId, "join").length,
    invites_sent_count: userId === "@spammer42:example.org" ? 140 : 2,
    events_sent_count: EVENTS_SENT[userId] ?? 0,
    rooms_created_count: userId === "@admin:example.org" ? 3 : 1,
    media_count: media.length,
    media_bytes: media.reduce((sum, m) => sum + m.size_bytes, 0),
    session_count: (userDevices[userId] ?? []).length,
  };
}

function taskId(): string {
  return `task_${Math.random().toString(36).slice(2, 10)}`;
}

/**
 * Starts `user.redact_events`: a running task whose progress follows the clock and which
 * succeeds after {@link REDACTION_RUN_MS}, as the server's would while it works through the
 * user's events newest first.
 */
export function startRedaction(
  userId: string,
  body: { room_id?: string; limit?: number },
  now = Date.now(),
): Task {
  let total = eventsLeft(userId);
  if (body.room_id) total = Math.min(total, 6);
  if (body.limit != null) total = Math.min(total, body.limit);
  const createdAt = new Date(now).toISOString();
  return putDrivenTask(
    {
      id: taskId(),
      action: "user.redact_events",
      status: "running",
      resource: { type: "user", id: userId },
      created_at: createdAt,
      started_at: createdAt,
      finished_at: null,
      scheduled_for: null,
      progress: { current: 0, total, unit: "events" },
      error: null,
      result: null,
    },
    (task) => {
      const elapsed = Date.now() - now;
      if (elapsed >= REDACTION_RUN_MS) {
        task.status = "succeeded";
        task.finished_at = new Date().toISOString();
        task.progress = { current: total, total, unit: "events" };
        task.result = { total, redacted: total, failed_count: 0, failed: [] };
        state.redacted[userId] = (state.redacted[userId] ?? 0) + total;
      } else {
        const current = Math.floor((total * elapsed) / REDACTION_RUN_MS);
        task.progress = { current, total, unit: "events" };
      }
    },
  );
}

/** `DELETE /users/{user_id}/media`: done at once, recorded as a finished task. */
export function deleteUserMedia(userId: string): Task {
  let deleted = 0;
  let bytes = 0;
  let skippedProtected = 0;
  for (const item of userMedia(userId)) {
    if (item.protected) {
      skippedProtected += 1;
      continue;
    }
    removeMedia(item);
    deleted += 1;
    bytes += item.size_bytes;
  }
  const user = findUser(userId);
  if (user) user.media_count = skippedProtected;
  const now = new Date().toISOString();
  return recordFinishedTask({
    id: taskId(),
    action: "media.delete",
    status: "succeeded",
    resource: { type: "user", id: userId },
    progress: { current: deleted, total: deleted + skippedProtected, unit: "items" },
    result: { deleted, bytes, skipped_protected: skippedProtected, failed: 0 },
    created_at: now,
    started_at: now,
    finished_at: now,
    scheduled_for: null,
    error: null,
  });
}
