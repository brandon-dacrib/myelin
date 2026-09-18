import { afterEach, describe, expect, it } from "vitest";
import { getAccessToken, getSession, hasScope, signIn, signOut } from "./auth";

afterEach(() => {
  signOut();
});

describe("auth", () => {
  it("has no session and no scopes before signing in", () => {
    expect(getSession()).toBeNull();
    expect(getAccessToken()).toBeNull();
    expect(hasScope("bridges:read")).toBe(false);
  });

  it("signs in against the mock issuer and grants the requested scopes", async () => {
    const session = await signIn(["admin:read", "bridges:read"]);
    expect(session.accessToken).toMatch(/^mock-admin-token\./);
    expect(hasScope("admin:read")).toBe(true);
    expect(hasScope("bridges:read")).toBe(true);
    expect(hasScope("bridges:write")).toBe(false);
  });

  it("treats admin:write as covering every scope check", async () => {
    await signIn(["admin:write"]);
    expect(hasScope("bridges:write")).toBe(true);
    expect(hasScope("moderation:write")).toBe(true);
  });

  it("treats bridges:write and moderation:write as implying their own :read", async () => {
    await signIn(["bridges:write", "moderation:write"]);
    expect(hasScope("bridges:read")).toBe(true);
    expect(hasScope("moderation:read")).toBe(true);
    expect(hasScope("admin:read")).toBe(false);
  });

  it("clears the session on sign out", async () => {
    await signIn(["admin:read"]);
    signOut();
    expect(getSession()).toBeNull();
    expect(hasScope("admin:read")).toBe(false);
  });
});
