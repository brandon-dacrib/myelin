import { describe, expect, it } from "vitest";
import { bridgeHealthMeta, formatBacklogEntry } from "./bridge-state";

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
