import { describe, expect, it } from "vitest";
import {
  catchUpExplanation,
  destinationHealth,
  destinationSeverity,
  forgetConsequence,
  formatInterval,
  formatSettingDuration,
  nextAttemptAt,
  parseDurationMs,
  pruneReasonLabel,
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

describe("formatSettingDuration", () => {
  it("reads the server's duration strings and millisecond counts", () => {
    expect(parseDurationMs("1w")).toBe(7 * 86_400_000);
    expect(parseDurationMs("1h30m")).toBe(5_400_000);
    expect(parseDurationMs("500")).toBe(500);
    expect(parseDurationMs("forever")).toBeUndefined();
    expect(formatSettingDuration("1w")).toBe("1 week");
    expect(formatSettingDuration("2w")).toBe("2 weeks");
    expect(formatSettingDuration("36h")).toBe("1.5 days");
    expect(formatSettingDuration("90m")).toBe("1.5 hours");
    expect(formatSettingDuration(86_400_000)).toBe("1 day");
  });

  it("is null for off and undefined for a value that is not a duration", () => {
    expect(formatSettingDuration("0s")).toBeNull();
    expect(formatSettingDuration(0)).toBeNull();
    expect(formatSettingDuration(true)).toBeUndefined();
    expect(formatSettingDuration(undefined)).toBeUndefined();
  });
});

describe("forgetConsequence", () => {
  it("names what is queued, the catch-up mark, and always the retry state and keys", () => {
    expect(forgetConsequence({ server_name: "a" })).toBe(
      "This drops its retry state and its cached signing keys. It is learned again from nothing the next time a room brings the two servers together.",
    );
    expect(
      forgetConsequence({
        server_name: "a",
        pending_pdu_count: 1,
        pending_edu_count: 2,
        catch_up_since: "x",
      }),
    ).toMatch(
      /^This drops 1 queued event \(unsent, lost\), 2 queued messages \(typing, receipts, device updates\), the note that it is behind and must be caught up, its retry state and its cached signing keys\./,
    );
  });
});

describe("pruneReasonLabel", () => {
  it("has words for every reason the server gives, and passes an unknown one through", () => {
    expect(pruneReasonLabel("unused")).toBe("Shares no room, nothing queued");
    expect(pruneReasonLabel("queued_for_current_rooms")).toBe(
      "Events queued for a room this server is in",
    );
    expect(pruneReasonLabel("something_new")).toBe("something_new");
  });
});
