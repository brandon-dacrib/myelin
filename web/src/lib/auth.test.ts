import { afterEach, describe, expect, it } from "vitest";
import { http, HttpResponse } from "msw";
import { server } from "@/mocks/node";
import {
  PASSWORD_LOGIN_OFF,
  getAccessToken,
  getSession,
  hasScope,
  signIn,
  signInWithPassword,
  signOut,
} from "./auth";

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

  it("treats admin:read as covering every :read, and no write (decision 0013)", async () => {
    await signIn(["admin:read"]);
    expect(hasScope("moderation:read")).toBe(true);
    expect(hasScope("bridges:read")).toBe(true);
    expect(hasScope("moderation:write")).toBe(false);
    expect(hasScope("admin:write")).toBe(false);
  });

  it("does not let moderation:read reach admin:read (message content)", async () => {
    await signIn(["moderation:read"]);
    expect(hasScope("moderation:read")).toBe(true);
    expect(hasScope("admin:read")).toBe(false);
  });

  it("clears the session on sign out", async () => {
    await signIn(["admin:read"]);
    signOut();
    expect(getSession()).toBeNull();
    expect(hasScope("admin:read")).toBe(false);
  });

  it("says password sign-in is turned off when the server refuses it so", async () => {
    server.use(
      http.post("*/_matrix/client/v3/login", () =>
        HttpResponse.json(
          { errcode: "M_FORBIDDEN", error: "Password login has been disabled on this server" },
          { status: 403 },
        ),
      ),
    );
    await expect(signInWithPassword("ops", "hunter2")).rejects.toThrow(PASSWORD_LOGIN_OFF);
  });
});
