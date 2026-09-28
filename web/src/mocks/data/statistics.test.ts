import { describe, expect, it } from "vitest";
import { timeseries } from "./statistics";

/** The mock follows the server's rules, so the pages are tested against what the server does. */
describe("mock timeseries", () => {
  const until = new Date().toISOString();
  const from = new Date(Date.now() - 30 * 86_400_000).toISOString();

  it("gives a counter a point for every step", () => {
    const series = timeseries(
      new URLSearchParams({ metric: "users.registered", step: "1d", from, until }),
    );
    expect("points" in series && series.points!.length).toBeGreaterThanOrEqual(30);
  });

  it("gives a gauge points only since sampling started", () => {
    const series = timeseries(
      new URLSearchParams({ metric: "users_count", step: "1d", from, until }),
    );
    expect("points" in series && series.points!.length).toBeLessThan(25);
  });

  it("refuses what the server refuses", () => {
    expect(timeseries(new URLSearchParams({ metric: "nope" }))).toHaveProperty(
      "pointer",
      "/metric",
    );
    expect(timeseries(new URLSearchParams({ metric: "users_count", step: "1q" }))).toHaveProperty(
      "pointer",
      "/step",
    );
    expect(
      timeseries(new URLSearchParams({ metric: "users_count", step: "1m", from, until })),
    ).toHaveProperty("pointer", "/step");
  });
});
