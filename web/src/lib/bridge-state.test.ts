import { describe, expect, it } from "vitest";
import { bridgeHealthMeta, describeQueue, formatBacklogEntry } from "./bridge-state";

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

  it("puts dead-lettered transactions in red", () => {
    expect(describeQueue({ pending: 1, dead_lettered: 2, oldest_pending_age_ms: 5_000 })).toEqual({
      text: "1 waiting, oldest 5s · 2 failed",
      status: "danger",
    });
  });

  it("reads as a dash from a server that sends no queue", () => {
    expect(describeQueue(undefined).text).toBe("—");
  });
});
