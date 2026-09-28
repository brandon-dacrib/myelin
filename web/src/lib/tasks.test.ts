import { describe, expect, it } from "vitest";
import { describeProgress, describeTaskAction, progressFraction } from "./tasks";

describe("tasks", () => {
  it("names known actions and spells out unknown ones", () => {
    expect(describeTaskAction("appservice.replay")).toBe("Replay bridge transactions");
    expect(describeTaskAction("media.rebuild_thumbnails")).toBe("Media rebuild thumbnails");
  });

  it("reads progress only when the task said how far it has to go", () => {
    expect(progressFraction({ progress: { current: 1, total: 4 } })).toBe(0.25);
    expect(progressFraction({ progress: { current: 9, total: 4 } })).toBe(1);
    expect(progressFraction({ progress: { current: 3 } })).toBeNull();
    expect(progressFraction({ progress: null })).toBeNull();
    expect(describeProgress({ progress: { current: 1200, total: 4000, unit: "files" } })).toBe(
      "1,200 of 4,000 files",
    );
    expect(describeProgress({ progress: { current: 3 } })).toBe("3");
    expect(describeProgress({})).toBeNull();
  });
});
