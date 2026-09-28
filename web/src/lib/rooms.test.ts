import { describe, expect, it } from "vitest";
import { describeRoomTaskResult, localInputToRfc3339, summarizeEvent } from "./rooms";

describe("summarizeEvent", () => {
  it("shows a message's body, a membership and a redaction", () => {
    expect(
      summarizeEvent({
        type: "m.room.message",
        content: { body: "hi" },
        state_key: null,
        redacted: false,
      }),
    ).toBe("hi");
    expect(
      summarizeEvent({
        type: "m.room.member",
        content: { membership: "join" },
        state_key: "@a:x",
        redacted: false,
      }),
    ).toBe("@a:x: join");
    expect(
      summarizeEvent({ type: "m.room.message", content: {}, state_key: null, redacted: true }),
    ).toBe("(redacted)");
  });
});

describe("describeRoomTaskResult", () => {
  it("words each room task's result", () => {
    expect(
      describeRoomTaskResult({
        action: "rooms.purge_history",
        status: "succeeded",
        result: { purged: 12, kept_state: 5, kept_local: 0 },
      }),
    ).toBe("Purged 12 events; kept 5 state and 0 local.");
    expect(
      describeRoomTaskResult({
        action: "rooms.delete",
        status: "succeeded",
        result: {
          kicked_users: ["@a:x", "@b:x"],
          failed_to_kick_users: [],
          local_aliases: [],
          new_room_id: null,
          blocked: true,
          purged: true,
          events_deleted: 40,
        },
      }),
    ).toBe("2 members removed, blocked, 40 events deleted.");
    expect(
      describeRoomTaskResult({
        action: "rooms.media.quarantine",
        status: "succeeded",
        result: { quarantined: 1, already_quarantined: 2, protected: 0 },
      }),
    ).toBe("Quarantined 1 item; 2 already were, 0 protected.");
  });

  it("says nothing for a task that has not succeeded", () => {
    expect(
      describeRoomTaskResult({ action: "rooms.delete", status: "running", result: null }),
    ).toBeNull();
  });
});

describe("localInputToRfc3339", () => {
  it("converts a datetime-local value and rejects an empty one", () => {
    expect(localInputToRfc3339("")).toBeNull();
    expect(localInputToRfc3339("2026-01-02T03:04")).toMatch(/^2026-01-0\dT\d\d:04:00\.000Z$/);
  });
});
