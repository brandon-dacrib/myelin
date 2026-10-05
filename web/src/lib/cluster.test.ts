import { describe, expect, it } from "vitest";
import type { Replica, Shard } from "@/api/cluster";
import {
  countByOwner,
  describeGeneration,
  describeOwnership,
  drainBlockedReason,
  groupByKind,
  isSingleNode,
  joinWithAnd,
  ownerColours,
} from "./cluster";

const replica = (id: string, status: Replica["status"] = "active", role = "replica"): Replica => ({
  id,
  status,
  role,
  shard_count: 0,
  epoch: 1,
  this_replica: false,
});
const shard = (id: string, owner: string | null, state: Shard["state"] = "owned"): Shard => ({
  id,
  kind: id.split("/")[0] as Shard["kind"],
  owner,
  state,
  epoch: 1,
});

describe("cluster wording", () => {
  it("knows a single node from the mode, or failing that from a replica's role", () => {
    expect(isSingleNode("single-node", undefined)).toBe(true);
    expect(isSingleNode("cluster", [replica("a", "active", "single-node")])).toBe(false);
    expect(isSingleNode(undefined, [replica("a", "active", "single-node")])).toBe(true);
    expect(isSingleNode(undefined, undefined)).toBe(false);
  });

  it("offers a drain only when another active replica could take the shards", () => {
    const all = [replica("a"), replica("b", "drained"), replica("c", "unreachable")];
    expect(drainBlockedReason(all[0], all, false)).toMatch(/No other replica is active/);
    expect(drainBlockedReason(all[0], [...all, replica("d")], false)).toBeNull();
    expect(drainBlockedReason(all[0], [...all, replica("d")], true)).toMatch(/single node/);
  });

  it("counts and describes ownership, unowned included", () => {
    const shards = [shard("room/0", "a"), shard("room/1", "b"), shard("room/2", null, "released")];
    expect(countByOwner(shards)).toEqual(
      new Map([
        ["a", 1],
        ["b", 1],
        [null, 1],
      ]),
    );
    expect(describeOwnership("room", shards)).toBe("3 room shards: a owns 1, b owns 1, 1 unowned");
    expect(describeOwnership("global", [shard("global/0", "a")])).toBe("1 global shard: a owns 1");
  });

  it("groups by kind in layout order and colours replicas first", () => {
    const shards = [shard("global/0", "z"), shard("user/0", "a"), shard("room/0", "b")];
    expect(groupByKind(shards).map((g) => g.kind)).toEqual(["room", "user", "global"]);
    expect([...ownerColours([replica("a"), replica("b")], shards).keys()]).toEqual(["a", "b", "z"]);
  });

  it("joins names with and", () => {
    expect(joinWithAnd(["a"])).toBe("a");
    expect(joinWithAnd(["a", "b"])).toBe("a and b");
    expect(joinWithAnd(["a", "b", "c"])).toBe("a, b and c");
  });
});

describe("describeGeneration", () => {
  it("reads a millisecond clock value as the start time, keeping the raw value for the tooltip", () => {
    const at = Date.UTC(2026, 9, 4, 18, 30, 5, 123);
    const g = describeGeneration(at);
    expect(g.startedAt?.toISOString()).toBe("2026-10-04T18:30:05.123Z");
    expect(g.title).toBe(`Generation ${at}: started 2026-10-04T18:30:05.123Z`);
    expect(g.label.length).toBeLessThan(20);
    expect(g.label).not.toContain(String(at));
  });

  it("keeps anything outside a timestamp's range a number", () => {
    expect(describeGeneration(7)).toEqual({ label: "7", title: "Generation 7", startedAt: null });
    expect(describeGeneration(123_456).startedAt).toBeNull();
  });
});
