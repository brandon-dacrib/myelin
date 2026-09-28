import type { ServerNotice } from "@/api/server-notices";

/**
 * The mock's server-notice history, newest first. Mutable module state -- sending appends --
 * restored after every Vitest test by {@link resetServerNotices} (`src/test/setup.ts`).
 */
export const SERVER_NOTICES_USER = "@server:example.org";

function seed(now = Date.now()): ServerNotice[] {
  return [
    {
      id: "notice-2",
      sender: SERVER_NOTICES_USER,
      type: "m.room.message",
      content: {
        msgtype: "m.text",
        body: "Your account was suspended for sending spam. Reply here if you think this is a mistake.",
      } as unknown as Record<string, never>,
      recipients: ["@spammer42:example.org"],
      room_ids: ["!notices-spammer42:example.org"],
      event_ids: ["$notice2-spammer42"],
      sent_at: new Date(now - 2 * 3_600_000).toISOString(),
    },
    {
      id: "notice-1",
      sender: SERVER_NOTICES_USER,
      type: "m.room.message",
      content: {
        msgtype: "m.text",
        body: "The server restarts for an upgrade on Saturday at 06:00 UTC. Expect five minutes without messages.",
      } as unknown as Record<string, never>,
      recipients: ["@admin:example.org", "@alice:example.org", "@bot:example.org"],
      room_ids: [
        "!notices-admin:example.org",
        "!notices-alice:example.org",
        "!notices-bot:example.org",
      ],
      event_ids: ["$notice1-admin", "$notice1-alice", "$notice1-bot"],
      sent_at: new Date(now - 3 * 24 * 3_600_000).toISOString(),
    },
  ];
}

export const serverNotices: ServerNotice[] = seed();

/** Puts the history back as it started. */
export function resetServerNotices(): void {
  serverNotices.splice(0, serverNotices.length, ...seed());
}
