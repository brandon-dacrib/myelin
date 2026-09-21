import { describe, expect, it } from "vitest";
import { formatCount, formatUptime, joinWithOr } from "./format";

describe("formatCount", () => {
  it("shows a real zero as zero, and a missing number as a dash", () => {
    expect(formatCount(0)).toBe("0");
    expect(formatCount(undefined)).toBe("—");
    expect(formatCount(null)).toBe("—");
  });

  it("groups thousands", () => {
    expect(formatCount(12345)).toBe((12345).toLocaleString());
  });
});

describe("formatUptime", () => {
  it("counts a new server's uptime in minutes rather than calling it 0h", () => {
    expect(formatUptime(20_000)).toBe("under a minute");
    expect(formatUptime(5 * 60_000)).toBe("5m");
    expect(formatUptime(59 * 60_000 + 59_000)).toBe("59m");
  });

  it("moves to hours, then days and hours", () => {
    expect(formatUptime(60 * 60_000)).toBe("1h");
    expect(formatUptime(23 * 3_600_000)).toBe("23h");
    expect(formatUptime((4 * 24 + 6) * 3_600_000)).toBe("4d 6h");
  });
});

describe("joinWithOr", () => {
  it("reads naturally for one, two and three names", () => {
    expect(joinWithOr([])).toBe("");
    expect(joinWithOr(["bridges"])).toBe("bridges");
    expect(joinWithOr(["bridges", "federation"])).toBe("bridges or federation");
    expect(joinWithOr(["bridges", "federation", "reports"])).toBe("bridges, federation or reports");
  });
});
