import { describe, expect, it } from "vitest";
import { ALL_SCOPES } from "./auth";
import { SCOPE_DESCRIPTIONS, describeScopes, normalizeScopes, redundantScopes } from "./scopes";

describe("scopes", () => {
  it("explains every scope the client knows, once, in catalog order", () => {
    expect(SCOPE_DESCRIPTIONS.map((d) => d.scope)).toEqual([...ALL_SCOPES]);
    for (const d of SCOPE_DESCRIPTIONS) {
      expect(d.grants.length).toBeGreaterThan(20);
      expect(d.area).not.toBe("");
    }
  });

  it("normalizes to catalog order without duplicates", () => {
    expect(normalizeScopes(["bridges:write", "admin:read", "bridges:write"])).toEqual([
      "admin:read",
      "bridges:write",
    ]);
  });

  it("names the scopes another one already implies", () => {
    expect(redundantScopes(["admin:write", "bridges:read"])).toEqual(["bridges:read"]);
    expect(redundantScopes(["admin:read", "admin:write"])).toEqual(["admin:read"]);
    expect(redundantScopes(["bridges:read", "bridges:write"])).toEqual(["bridges:read"]);
    expect(redundantScopes(["admin:read", "moderation:read"])).toEqual(["moderation:read"]);
    expect(redundantScopes(["bridges:read", "moderation:write"])).toEqual([]);
  });

  it("describes a list briefly", () => {
    expect(describeScopes(["admin:read", "admin:write"])).toBe("Full administrator");
    expect(describeScopes(["moderation:write", "bridges:read"])).toBe(
      "bridges:read, moderation:write",
    );
  });
});
