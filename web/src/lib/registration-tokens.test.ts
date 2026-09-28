import { describe, expect, it } from "vitest";
import {
  customTokenProblem,
  expiryFor,
  formatExpiry,
  formatUses,
  inviteLink,
  parseUses,
  tokenStatus,
  type RegistrationTokenView,
} from "./registration-tokens";

const NOW = Date.parse("2026-09-27T12:00:00Z");
const DAY = 86_400_000;

function token(over: Partial<RegistrationTokenView>): RegistrationTokenView {
  return {
    token: "t",
    valid: true,
    usesAllowed: null,
    pending: 0,
    completed: 0,
    expiresAt: null,
    createdAt: null,
    ...over,
  };
}

describe("tokenStatus", () => {
  it("takes the server's word for a valid token, whatever this clock says", () => {
    const t = token({ valid: true, expiresAt: new Date(NOW - DAY).toISOString() });
    expect(tokenStatus(t, NOW)).toEqual({ kind: "valid", label: "Valid" });
  });

  it("explains an invalid token by its expiry first", () => {
    const t = token({
      valid: false,
      usesAllowed: 1,
      completed: 1,
      expiresAt: new Date(NOW - DAY).toISOString(),
    });
    expect(tokenStatus(t, NOW).kind).toBe("expired");
  });

  it("says a token is used up once its accounts are made", () => {
    expect(tokenStatus(token({ valid: false, usesAllowed: 2, completed: 2 }), NOW)).toMatchObject({
      kind: "used-up",
      label: "Used up",
    });
  });

  it("says when the last use is still being registered", () => {
    const status = tokenStatus(token({ valid: false, usesAllowed: 1, pending: 1 }), NOW);
    expect(status).toMatchObject({ kind: "reserved", label: "Uses in progress" });
    expect(status.detail).toBe("1 registration is still finishing with it.");
  });

  it("falls back to plain 'Not valid' when nothing explains it", () => {
    expect(tokenStatus(token({ valid: false }), NOW)).toMatchObject({ kind: "invalid" });
  });
});

describe("formatUses", () => {
  it("counts against the limit, or says there is none", () => {
    expect(formatUses({ completed: 3, usesAllowed: 5 })).toBe("3 of 5");
    expect(formatUses({ completed: 3, usesAllowed: null })).toBe("3 of unlimited");
  });
});

describe("inviteLink", () => {
  it("points at the public registration page and escapes the token", () => {
    expect(inviteLink("abc~1", "https://hs.example")).toBe(
      "https://hs.example/admin/register?token=abc~1",
    );
    expect(inviteLink("a b", "https://hs.example")).toBe(
      "https://hs.example/admin/register?token=a%20b",
    );
  });
});

describe("customTokenProblem", () => {
  it("accepts the characters the server does and refuses the rest", () => {
    expect(customTokenProblem("Spring_2026.cohort~a-b")).toBeNull();
    expect(customTokenProblem("")).toMatch(/Type the token/);
    expect(customTokenProblem("has space")).toMatch(/Only letters/);
    expect(customTokenProblem("x".repeat(65))).toMatch(/At most 64/);
  });
});

describe("parseUses", () => {
  it("takes a whole number and nothing else", () => {
    expect(parseUses(" 3 ")).toEqual({ value: 3 });
    expect(parseUses("0")).toEqual({ value: 0 });
    expect(parseUses("1.5")).toHaveProperty("error");
    expect(parseUses("")).toHaveProperty("error");
  });
});

describe("expiryFor", () => {
  it("turns each choice into an instant, or null for never", () => {
    expect(expiryFor("never", "", NOW)).toBeNull();
    expect(expiryFor("7d", "", NOW)).toBe(new Date(NOW + 7 * DAY).toISOString());
    expect(expiryFor("custom", "", NOW)).toBeUndefined();
    expect(expiryFor("custom", "2026-10-01T09:30", NOW)).toBe(
      new Date("2026-10-01T09:30").toISOString(),
    );
  });
});

describe("formatExpiry", () => {
  it("says when, relative to now, within a month either way", () => {
    expect(formatExpiry(null, NOW)).toBe("Never");
    expect(formatExpiry(new Date(NOW + 6 * DAY).toISOString(), NOW)).toBe("in 6 days");
    expect(formatExpiry(new Date(NOW + 3 * 3_600_000).toISOString(), NOW)).toBe("in 3 h");
    expect(formatExpiry(new Date(NOW - 2 * DAY).toISOString(), NOW)).toBe("2 days ago");
    expect(formatExpiry(new Date(NOW + 30 * DAY).toISOString(), NOW)).toBe("in 30 days");
    expect(formatExpiry(new Date(NOW + 60 * DAY).toISOString(), NOW)).not.toMatch(/^in /);
  });
});
