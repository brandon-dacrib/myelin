import { describe, expect, it } from "vitest";
import { STREAMS, WHAT_DOES_NOT_MOVE, streamLabel } from "./migration";

describe("streamLabel", () => {
  it("names every stream the importer copies in words", () => {
    // `hs_compat::migration::Stream::ALL`'s wire names, in copy order.
    const wire = [
      "users",
      "devices",
      "access_tokens",
      "account_data",
      "e2e_keys",
      "cross_signing",
      "key_backups",
      "push_rules",
      "pushers",
      "filters",
      "rooms",
      "receipts",
      "media",
    ];
    for (const name of wire) {
      expect(STREAMS[name]?.explanation, name).toBeTruthy();
      expect(streamLabel(name)).not.toContain("_");
    }
    expect(streamLabel("e2e_keys")).toBe("Device encryption keys");
  });

  it("never shows a wire name, even for a stream this build does not know", () => {
    expect(streamLabel("room_directory")).toBe("Room directory");
    expect(streamLabel("migration")).toBe("Migration");
    expect(streamLabel(undefined)).toBe("Unknown");
  });
});

describe("WHAT_DOES_NOT_MOVE", () => {
  it("keeps the runbook's list, each with why", () => {
    const titles = WHAT_DOES_NOT_MOVE.map((item) => item.title);
    expect(titles).toContain("Other servers' media");
    expect(titles).toContain("Presence");
    expect(titles).toContain("Bridges");
    for (const item of WHAT_DOES_NOT_MOVE) expect(item.detail.length).toBeGreaterThan(40);
  });
});
