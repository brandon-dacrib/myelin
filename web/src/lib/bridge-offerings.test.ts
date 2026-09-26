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
