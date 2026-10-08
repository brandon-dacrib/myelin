import { describe, expect, it } from "vitest";
import {
  defaultOfferingOptions,
  frontDoorSentence,
  imageTag,
  instanceCountList,
  instanceStateLabel,
  looksLikeUserId,
  parseUserList,
  requestFromOffering,
  settingsEffects,
  totalInstances,
} from "./bridge-offerings";

describe("imageTag", () => {
  it("reads the tag after the last path segment", () => {
    expect(imageTag("dock.mau.dev/mautrix/whatsapp:v0.12.1")).toBe("v0.12.1");
    expect(imageTag("localhost:5000/whatsapp")).toBe("latest");
    expect(imageTag("localhost:5000/whatsapp:edge")).toBe("edge");
    expect(imageTag("ghcr.io/x/y@sha256:abc")).toBe("latest");
    expect(imageTag(undefined)).toBe("latest");
  });
});

describe("instance counts", () => {
  it("orders by the state machine and drops zeroes", () => {
    expect(instanceCountList({ failed: 1, ready: 3, starting: 0, requested: 2, odd: 1 })).toEqual([
      { state: "requested", count: 2 },
      { state: "ready", count: 3 },
      { state: "failed", count: 1 },
      { state: "odd", count: 1 },
    ]);
    expect(totalInstances({ ready: 3, failed: 1 })).toBe(4);
    expect(totalInstances(undefined)).toBe(0);
  });

  it("labels a state this build does not know", () => {
    expect(instanceStateLabel("ready")).toBe("Ready");
    expect(instanceStateLabel("hibernating")).toBe("Hibernating");
  });
});

describe("defaults and edits", () => {
  it("switches on what the catalogue says the bridge can do", () => {
    expect(
      defaultOfferingOptions({
        supports_double_puppeting: true,
        required_features: ["org.matrix.msc3202"],
        renders_config: true,
      }),
    ).toEqual({ encryption: true, double_puppeting: true, backfill: true });
    expect(defaultOfferingOptions({})).toEqual({
      encryption: false,
      double_puppeting: false,
      backfill: false,
    });
  });

  it("round-trips an offering into the request that keeps it", () => {
    expect(
      requestFromOffering({
        type: "mautrix-whatsapp",
        mode: "per_user",
        enabled: true,
        runtime: "cluster",
        image: "dock.mau.dev/mautrix/whatsapp:v0.12.1",
        access: { all_local_users: false, users: ["@a:x"] },
        options: { encryption: true },
      }),
    ).toEqual({
      enabled: true,
      runtime: "cluster",
      image_tag: "v0.12.1",
      access: { all_local_users: false, users: ["@a:x"] },
      options: { encryption: true, double_puppeting: false, backfill: false },
    });
  });
});

describe("front door", () => {
  it("tells the administrator what to tell people", () => {
    expect(
      frontDoorSentence({
        type: "mautrix-whatsapp",
        name: "WhatsApp",
        mode: "per_user",
        front_door: "@whatsappbot:example.org",
        access: { all_local_users: true },
      }),
    ).toBe("Anyone here can message @whatsappbot:example.org to get their own WhatsApp bridge.");
    expect(
      frontDoorSentence({ type: "heisenbridge", mode: "shared", front_door: null }),
    ).toBeNull();
  });
});

describe("user lists", () => {
  it("splits, trims and deduplicates", () => {
    expect(parseUserList("@a:x, @b:x\n@a:x  @c:y")).toEqual(["@a:x", "@b:x", "@c:y"]);
  });
  it("checks the shape of a user id", () => {
    expect(looksLikeUserId("@alice:example.org")).toBe(true);
    expect(looksLikeUserId("alice")).toBe(false);
    expect(looksLikeUserId("@alice")).toBe(false);
  });
});

describe("deploymentPhaseMeta", () => {
  it("reads the operator's phases in words, and leaves an unknown one as it came", async () => {
    const { deploymentPhaseMeta } = await import("./bridge-offerings");
    expect(deploymentPhaseMeta("Ready")).toEqual({ label: "Running", status: "success" });
    expect(deploymentPhaseMeta("Pending")).toEqual({ label: "Starting", status: "info" });
    expect(deploymentPhaseMeta("Degraded")).toEqual({
      label: "Not running properly",
      status: "danger",
    });
    expect(deploymentPhaseMeta("Evicted")).toEqual({ label: "Evicted", status: "neutral" });
  });
});

describe("settingsEffects", () => {
  it("says who-can-have-one applies from now on", () => {
    expect(settingsEffects("cluster", "cluster", 3).access).toMatch(
      /^Applies to people who ask from now on\./,
    );
  });

  it("says the image and options reach existing bridges, and how, by runtime", () => {
    expect(settingsEffects("cluster", "cluster", 0).imageAndOptions).toBe(
      "Applies to every bridge created after saving, and to any people already have.",
    );
    expect(settingsEffects("cluster", "cluster", 1).imageAndOptions).toMatch(
      /^Applies to the 1 bridge people already have as well as to new ones: the server redeploys/,
    );
    expect(settingsEffects("elsewhere", "elsewhere", 2).imageAndOptions).toMatch(
      /The 2 bridges people already have run elsewhere: each keeps its old settings until its files are downloaded again/,
    );
  });

  it("warns what a changed runtime does to existing bridges, and nothing when none exist", () => {
    expect(settingsEffects("cluster", "cluster", 2).runtimeChange).toBeNull();
    expect(settingsEffects("elsewhere", "cluster", 0).runtimeChange).toBeNull();
    expect(settingsEffects("elsewhere", "cluster", 2).runtimeChange).toMatch(
      /deploys each into the cluster on its next pass\. Stop any copy run by hand first/,
    );
    expect(settingsEffects("cluster", "elsewhere", 1).runtimeChange).toMatch(
      /^Saving does not move the 1 bridge people already have: the ones in the cluster keep running there/,
    );
  });
});
