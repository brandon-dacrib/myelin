import { describe, expect, it } from "vitest";
import { deriveDisplayName, deriveKindLabel, isBuiltInAppservice } from "./bridges";

describe("the server's own bridge manager", () => {
  it("is recognised by the id the server registers it under", () => {
    expect(isBuiltInAppservice({ id: "myelin-bridges" })).toBe(true);
    expect(isBuiltInAppservice({ id: "whatsapp" })).toBe(false);
  });

  it("is named as the server's own, not humanised from its id", () => {
    expect(deriveDisplayName({ id: "myelin-bridges" })).toBe("This server's bridge manager");
    expect(deriveKindLabel({ id: "myelin-bridges", protocols: [] })).toBe(
      "Built in: runs the bridges offered here",
    );
  });

  it("leaves every other appservice as before", () => {
    expect(deriveDisplayName({ id: "work-whatsapp" })).toBe("Work Whatsapp");
    expect(deriveKindLabel({ id: "work-whatsapp", protocols: [] })).toBe("Custom appservice");
    expect(deriveKindLabel({ protocols: ["irc"] })).toBe("irc");
  });
});
