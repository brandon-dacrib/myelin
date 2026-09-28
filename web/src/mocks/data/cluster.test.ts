import { describe, expect, it } from "vitest";
import {
  clusterSummary,
  drainReplica,
  getReplica,
  listReplicas,
  listShards,
  settleCluster,
  undrainReplica,
} from "./cluster";
import { getTask, listTasks } from "./tasks";

const owners = () => listShards(null).map((s) => s.owner);

describe("the mock cluster", () => {
  it("agrees with GET /cluster", () => {
    expect(clusterSummary()).toMatchObject({
      mode: "cluster",
      replica_count: listReplicas().length,
      shard_count: listShards(null).length,
    });
    expect(listReplicas().filter((r) => r.this_replica)).toHaveLength(1);
    const counted = listReplicas().reduce((n, r) => n + (r.shard_count ?? 0), 0);
    expect(counted).toBe(listShards(null).length);
  });

  it("drains over time, with a task on the Tasks page that finishes with it", () => {
    const start = Date.now();
    const owned = listShards(null).filter((s) => s.owner === "hs-1").length;
    const outcome = drainReplica("hs-1", "@op:example.org", start);
    if (!("replica" in outcome)) throw new Error("drain refused");
    expect(outcome.replica).toMatchObject({
      status: "draining",
      drain_requested_by: "@op:example.org",
    });
    const taskId = outcome.replica.drain_task_id!;
    expect(listTasks(new URLSearchParams("action=cluster."))[0]).toMatchObject({
      id: taskId,
      status: "running",
      resource: { type: "replica", id: "hs-1" },
      progress: { current: 0, total: owned, unit: "shards" },
    });

    settleCluster(start + 2_500);
    const halfway = getTask(taskId)!;
    expect(halfway.progress?.current).toBe(Math.floor(owned / 2));
    expect(listShards(null).some((s) => s.state === "released")).toBe(true);

    settleCluster(start + 5_000);
    expect(getReplica("hs-1")).toMatchObject({ status: "drained", shard_count: 0 });
    expect(getTask(taskId)).toMatchObject({ status: "succeeded" });
    expect(owners()).not.toContain("hs-1");
    expect(owners()).not.toContain(null);
  });

  it("gives an undrained replica back what it handed off, and cancels a running drain", () => {
    const start = Date.now();
    const before = owners();
    const outcome = drainReplica("hs-2", undefined, start);
    if (!("replica" in outcome)) throw new Error("drain refused");
    settleCluster(start + 1_000);
    const undrained = undrainReplica("hs-2", start + 1_000);
    expect(undrained).toMatchObject({ replica: { status: "active", drain_task_id: null } });
    expect(getTask(outcome.replica.drain_task_id!)).toMatchObject({ status: "cancelled" });
    expect(owners()).toEqual(before);
    // Undraining an active replica changes nothing.
    expect(undrainReplica("hs-2")).toMatchObject({ replica: { status: "active" } });
  });

  it("refuses to drain the last active replica, and does not know a stranger", () => {
    const start = Date.now();
    drainReplica("hs-1", undefined, start);
    drainReplica("hs-2", undefined, start);
    const refused = drainReplica("hs-0", undefined, start);
    expect(refused).toMatchObject({ problem: "conflict" });
    expect("detail" in refused && refused.detail).toMatch(/no other replica is active/);
    expect(drainReplica("hs-9")).toMatchObject({ problem: "not-found" });
    expect(undrainReplica("hs-9")).toMatchObject({ problem: "not-found" });
    // Draining one already draining is answered as it is.
    expect(drainReplica("hs-1", undefined, start)).toMatchObject({
      replica: { status: "draining" },
    });
  });
});
