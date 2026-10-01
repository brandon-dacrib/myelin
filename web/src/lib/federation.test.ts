import { describe, expect, it } from "vitest";
import {
  catchUpExplanation,
  destinationHealth,
  destinationSeverity,
  formatInterval,
  nextAttemptAt,
} from "./federation";

describe("destinationHealth", () => {
  it("names failing, backing off and healthy, each with what it means", () => {
    expect(destinationHealth({ failing_since: "2026-10-01T00:00:00Z" }).label).toBe("Failing");
    expect(destinationHealth({ retry_interval_ms: 60_000 }).label).toBe("Backing off");
    const healthy = destinationHealth({});
    expect(healthy.label).toBe("Healthy");
    expect(healthy.explanation).toMatch(/succeeded/);
  });
});

describe("destinationSeverity", () => {
  it("puts failing first, then catching up, then backing off, then healthy", () => {
    const failing = destinationSeverity({ failing_since: "x" });
    const catching = destinationSeverity({ catch_up_since: "x" });
    const backing = destinationSeverity({ retry_interval_ms: 1 });
    const healthy = destinationSeverity({});
    expect([failing, catching, backing, healthy]).toEqual([0, 1, 2, 3]);
  });
});

describe("nextAttemptAt", () => {
  const last = "2026-10-01T10:00:00.000Z";
  it("is the last attempt plus the interval", () => {
    const now = Date.parse("2026-10-01T10:01:00.000Z");
    expect(nextAttemptAt({ retry_last_at: last, retry_interval_ms: 300_000 }, now)).toEqual({
      at: "2026-10-01T10:05:00.000Z",
      due: false,
    });
  });
  it("is due once that time has passed", () => {
    const now = Date.parse("2026-10-01T11:00:00.000Z");
    expect(nextAttemptAt({ retry_last_at: last, retry_interval_ms: 300_000 }, now)?.due).toBe(true);
  });
  it("is nothing while the destination is not backing off", () => {
    expect(nextAttemptAt({ retry_last_at: last, retry_interval_ms: null })).toBeNull();
    expect(nextAttemptAt({ retry_last_at: null, retry_interval_ms: 1000 })).toBeNull();
  });
});

describe("formatInterval", () => {
  it("says an interval as a person would", () => {
    expect(formatInterval(30_000)).toBe("30 seconds");
    expect(formatInterval(60_000)).toBe("1 minute");
    expect(formatInterval(90_000)).toBe("1.5 minutes");
    expect(formatInterval(3_600_000)).toBe("1 hour");
    expect(formatInterval(2 * 86_400_000)).toBe("2 days");
  });
});

describe("catchUpExplanation", () => {
  it("names the queue limit and says nothing is lost", () => {
    const text = catchUpExplanation(10_000);
    expect(text).toContain("10,000 events");
    expect(text).toContain("latest event of each of those rooms");
    expect(text).toContain("Nothing is lost");
  });
});
