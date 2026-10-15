import { describe, expect, it } from "vitest";
import {
  checkVolumeSize,
  currentVolumeSize,
  describeVolumeUsage,
  quantityBytes,
  suggestedVolumeSize,
  volumeMessage,
  volumeNeedsAttention,
  volumeUsedRatio,
} from "./bridge-volume";

const GiB = 2 ** 30;

describe("bridge volume words", () => {
  it("says how full a measured volume is, and nothing before the first measurement", () => {
    expect(
      describeVolumeUsage({ used_bytes: 0.98 * GiB, capacity_bytes: GiB, used_ratio: 0.98 }),
    ).toBe("1003.5 MiB of 1.0 GiB used (98%)");
    expect(describeVolumeUsage({ used_bytes: null, capacity_bytes: null, used_ratio: null })).toBe(
      null,
    );
    // A server that sent bytes but no ratio: worked out here.
    expect(volumeUsedRatio({ used_bytes: GiB / 4, capacity_bytes: GiB, used_ratio: null })).toBe(
      0.25,
    );
  });

  it("flags nearly full and full volumes only", () => {
    expect(volumeNeedsAttention({ state: "full" })).toBe(true);
    expect(volumeNeedsAttention({ state: "nearly_full" })).toBe(true);
    expect(volumeNeedsAttention({ state: "ok" })).toBe(false);
    expect(volumeNeedsAttention({ state: "unknown" })).toBe(false);
    expect(volumeNeedsAttention(null)).toBe(false);
  });

  it("uses the server's message, with a fallback for a full volume without one", () => {
    expect(volumeMessage({ state: "full", message: "No space left." })).toBe("No space left.");
    expect(volumeMessage({ state: "full", message: null })).toMatch(/cannot write and stops/);
    expect(volumeMessage({ state: "ok", message: null })).toBe(null);
  });

  it("reads the size the volume has now from the cluster when someone raised it by hand", () => {
    expect(currentVolumeSize({ size: "5Gi", cluster_size: "20Gi" })).toBe("20Gi");
    expect(currentVolumeSize({ size: "5Gi", cluster_size: null })).toBe("5Gi");
  });

  it("checks how a size is written, and leaves the range to the server", () => {
    expect(checkVolumeSize(" 10Gi ")).toEqual({ size: "10Gi" });
    expect(checkVolumeSize("512Mi")).toEqual({ size: "512Mi" });
    expect(checkVolumeSize("")).toEqual({ error: "Enter a size, such as 10Gi." });
    expect(checkVolumeSize("10GB")).toHaveProperty("error");
    expect(checkVolumeSize("1.5Gi")).toHaveProperty("error");
  });

  it("reads Kubernetes quantities and suggests double the current size", () => {
    expect(quantityBytes("5Gi")).toBe(5 * GiB);
    expect(quantityBytes("1500M")).toBe(1.5e9);
    expect(quantityBytes("lots")).toBe(null);
    expect(suggestedVolumeSize({ size: "1Gi", cluster_size: null })).toBe("2Gi");
    expect(suggestedVolumeSize({ size: "5Gi", cluster_size: "8Gi" })).toBe("16Gi");
    expect(suggestedVolumeSize({ size: "800Gi", cluster_size: null })).toBe("1Ti");
  });
});
