import { describe, expect, it } from "vitest";
import type { Report } from "@/api/reports";
import { describeScore, reportSubject, resolutionLabel } from "./reports";

const base: Report = {
  id: "r",
  kind: "event",
  status: "open",
  reporter_id: "@a:example.org",
  received_at: "2026-09-27T00:00:00Z",
};

describe("reports", () => {
  it("says what was reported in one line", () => {
    expect(reportSubject({ ...base, room_id: "!r:example.org" })).toBe("Message in !r:example.org");
    expect(reportSubject({ ...base, kind: "room", room_id: "!r:example.org" }, "General")).toBe(
      "Room General",
    );
    expect(reportSubject({ ...base, kind: "user", reported_user_id: "@b:example.org" })).toBe(
      "User @b:example.org",
    );
  });

  it("words scores and resolutions", () => {
    expect(describeScore(null)).toBe("No score");
    expect(describeScore(-100)).toBe("-100 (very offensive)");
    expect(describeScore(0)).toBe("0 (inoffensive)");
    expect(resolutionLabel("room_blocked")).toBe("Blocked the room");
    expect(resolutionLabel(null)).toBe("—");
  });
});
