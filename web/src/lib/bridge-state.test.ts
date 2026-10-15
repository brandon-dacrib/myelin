import { describe, expect, it } from "vitest";
import {
  DEAD_LETTER_HINT,
  bridgeHealthMeta,
  describeBacklogContents,
  describeQueue,
  formatBacklogEntry,
  oldestDeadLetterAgeMs,
  shortRoomId,
} from "./bridge-state";

describe("formatBacklogEntry", () => {
  it("formats a pending entry's age in minutes", () => {
    expect(formatBacklogEntry(120_000, false)).toBe("Pending, 2 min old");
  });

  it("formats a dead-lettered entry's age in hours", () => {
    expect(formatBacklogEntry(9_600_000, true)).toBe("Dead-lettered, 3 h old");
  });
});

describe("bridgeHealthMeta", () => {
  it("maps every AppService.health value to a badge status and a human label", () => {
    const statuses = ["healthy", "degraded", "down", "paused", "unknown"] as const;
    for (const status of statuses) {
      expect(bridgeHealthMeta[status]).toBeDefined();
      expect(bridgeHealthMeta[status].label).not.toBe("");
    }
  });

  it("treats down as danger and healthy as success", () => {
    expect(bridgeHealthMeta.down.status).toBe("danger");
    expect(bridgeHealthMeta.healthy.status).toBe("success");
  });

  it("treats paused as muted", () => {
    expect(bridgeHealthMeta.paused.status).toBe("muted");
  });
});

describe("describeQueue", () => {
  it("says a bridge with nothing waiting is up to date", () => {
    expect(describeQueue({ pending: 0, dead_lettered: 0, oldest_pending_age_ms: null })).toEqual({
      text: "Up to date",
      status: "success",
    });
  });

  it("says how much waits and for how long, and warns past a minute", () => {
    expect(describeQueue({ pending: 3, dead_lettered: 0, oldest_pending_age_ms: 4_000 })).toEqual({
      text: "3 waiting, oldest 4s",
      status: "neutral",
    });
    expect(
      describeQueue({ pending: 12, dead_lettered: 0, oldest_pending_age_ms: 130_000 }).status,
    ).toBe("warning");
  });

  it("names dead-lettered transactions as such, in red, and says where they are dealt with", () => {
    expect(describeQueue({ pending: 1, dead_lettered: 2, oldest_pending_age_ms: 5_000 })).toEqual({
      text: "1 waiting, oldest 5s · 2 dead-lettered",
      status: "danger",
      hint: DEAD_LETTER_HINT,
    });
    expect(DEAD_LETTER_HINT).toMatch(/replays or discards them on the bridge's page/);
  });

  it("gives no hint when nothing is dead-lettered", () => {
    expect(
      describeQueue({ pending: 3, dead_lettered: 0, oldest_pending_age_ms: 4_000 }).hint,
    ).toBeUndefined();
  });

  it("reads as a dash from a server that sends no queue", () => {
    expect(describeQueue(undefined).text).toBe("—");
  });
});

describe("describeBacklogContents", () => {
  it("says what an entry carries, leaving out what it has none of", () => {
    expect(describeBacklogContents({ events: 12, ephemeral: 3, to_device: 1 })).toBe(
      "12 events · 3 ephemeral · 1 to-device",
    );
    expect(describeBacklogContents({ events: 1, ephemeral: 0, to_device: 0 })).toBe("1 event");
    expect(describeBacklogContents({ events: 0, ephemeral: 4, to_device: 0 })).toBe("4 ephemeral");
  });

  it("is empty for a server that does not say", () => {
    expect(describeBacklogContents({})).toBe("");
  });
});

describe("shortRoomId", () => {
  it("cuts the opaque part and keeps the server", () => {
    expect(shortRoomId("!OGEhHVWSdvArJzumhm:matrix.org")).toBe("!OGEhHV…:matrix.org");
  });

  it("leaves a short id, or one with no server, alone", () => {
    expect(shortRoomId("!family:example.org")).toBe("!family:example.org");
    expect(shortRoomId("!signalnotes:example.org")).toBe("!signalnotes:example.org");
    expect(shortRoomId("!abcdefghijklmnop")).toBe("!abcdefghijklmnop");
  });
});

describe("oldestDeadLetterAgeMs", () => {
  it("is the oldest dead-lettered entry's age, ignoring waiting ones", () => {
    expect(
      oldestDeadLetterAgeMs([
        { age_ms: 9_000_000, dead_lettered: false },
        { age_ms: 3_000, dead_lettered: true },
        { age_ms: 7_000, dead_lettered: true },
      ]),
    ).toBe(7_000);
  });

  it("is undefined with no dead letters", () => {
    expect(oldestDeadLetterAgeMs([{ age_ms: 5, dead_lettered: false }])).toBeUndefined();
  });
});
