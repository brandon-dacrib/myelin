import { afterEach, describe, expect, it } from "vitest";
import { heartbeatTrend, recordHeartbeats, resetHeartbeatHistory, type Replica } from "./cluster";

const replica = (id: string, heartbeat_seq: number | undefined): Replica => ({
  id,
  role: "replica",
  status: "active",
  shard_count: 1,
  epoch: 1,
  this_replica: false,
  heartbeat_seq,
});

/** The page's own readings of each replica's heartbeat sequence, and what they say. */
describe("heartbeat trend", () => {
  afterEach(() => resetHeartbeatHistory());

  it("says nothing from one reading", () => {
    recordHeartbeats([replica("a", 10)], 1_000);
    expect(heartbeatTrend("a")).toEqual({
      samples: [{ at: 1_000, seq: 10 }],
      increments: [],
      advancing: null,
      lastAdvanceAt: 1_000,
    });
    expect(heartbeatTrend("nobody").advancing).toBeNull();
  });

  it("counts the heartbeats between readings and says the sequence is advancing", () => {
    recordHeartbeats([replica("a", 10)], 1_000);
    recordHeartbeats([replica("a", 17)], 16_000);
    recordHeartbeats([replica("a", 25)], 31_000);
    const trend = heartbeatTrend("a");
    expect(trend.increments).toEqual([7, 8]);
    expect(trend.advancing).toBe(true);
    expect(trend.lastAdvanceAt).toBe(31_000);
  });

  it("says since when a sequence stopped moving", () => {
    recordHeartbeats([replica("a", 10)], 1_000);
    recordHeartbeats([replica("a", 17)], 16_000);
    recordHeartbeats([replica("a", 17)], 31_000);
    recordHeartbeats([replica("a", 17)], 46_000);
    const trend = heartbeatTrend("a");
    expect(trend.increments).toEqual([7, 0, 0]);
    expect(trend.advancing).toBe(false);
    expect(trend.lastAdvanceAt).toBe(16_000);
  });

  it("skips a replica with no sequence, keeps replicas apart, and forgets the oldest readings", () => {
    for (let i = 0; i < 50; i += 1) {
      recordHeartbeats([replica("a", i), replica("b", 100 - i), replica("single", undefined)], i);
    }
    expect(heartbeatTrend("single").samples).toHaveLength(0);
    expect(heartbeatTrend("a").samples).toHaveLength(40);
    expect(heartbeatTrend("a").samples[0]).toEqual({ at: 10, seq: 10 });
    // A sequence that goes down (a replica restarted and started over) is not a negative count.
    expect(heartbeatTrend("b").increments.every((n) => n === 0)).toBe(true);
    expect(heartbeatTrend("b").advancing).toBe(false);
  });

  it("does not record the same moment twice", () => {
    recordHeartbeats([replica("a", 10)], 1_000);
    recordHeartbeats([replica("a", 11)], 1_000);
    expect(heartbeatTrend("a").samples).toHaveLength(1);
  });
});
